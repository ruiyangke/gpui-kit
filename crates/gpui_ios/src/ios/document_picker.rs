//! GPUI's open and save prompts on iPad.
//!
//! Opening uses the system document picker. Files come in as copies in the
//! app's container, so the paths it returns can be read with no security
//! scope; folders open in place. iPadOS has no prompt for a path to write to: a
//! new file goes into the app's Documents folder, which the Files app
//! shows as "On My iPad › <app>".
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::PathPromptOptions;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject},
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSString, NSURL};
use objc2_ui_kit::UIViewController;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
};

type Reply = oneshot::Sender<Result<Option<Vec<PathBuf>>>>;

pub(super) struct DelegateIvars {
    reply: RefCell<Option<Reply>>,
    in_place: bool,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[name = "GPUIIosDocumentPickerDelegate"]
    #[ivars = DelegateIvars]
    pub(super) struct PickerDelegate;

    unsafe impl NSObjectProtocol for PickerDelegate {}

    impl PickerDelegate {
        #[unsafe(method(documentPicker:didPickDocumentsAtURLs:))]
        fn did_pick(&self, _picker: &AnyObject, urls: &NSArray<NSURL>) {
            let paths = urls
                .iter()
                .filter_map(|url| {
                    if self.ivars().in_place {
                        // Never balanced by a stop: the app reads the folder
                        // for as long as it runs, and the scope ends with it.
                        let _: bool = unsafe { msg_send![&*url, startAccessingSecurityScopedResource] };
                    }
                    url.path().map(|path| PathBuf::from(path.to_string()))
                })
                .collect();
            self.answer(Ok(Some(paths)));
        }

        #[unsafe(method(documentPickerWasCancelled:))]
        fn was_cancelled(&self, _picker: &AnyObject) {
            self.answer(Ok(None));
        }
    }
);

impl PickerDelegate {
    fn answer(&self, result: Result<Option<Vec<PathBuf>>>) {
        if let Some(reply) = self.ivars().reply.borrow_mut().take() {
            let _ = reply.send(result);
        }
        CURRENT.with(|current| current.borrow_mut().take());
    }
}

thread_local! {
    /// The picker holds its delegate weakly: this keeps the open one alive.
    static CURRENT: RefCell<Option<Retained<PickerDelegate>>> = const { RefCell::new(None) };
}

/// Presents the document picker over `controller` and answers `reply`
/// with the chosen paths, or `None` when the person cancels.
pub(super) fn open(controller: &UIViewController, options: &PathPromptOptions, reply: Reply) {
    let Some(main_thread) = MainThreadMarker::new() else {
        let _ = reply.send(Err(anyhow!("The document picker needs the main thread")));
        return;
    };
    let folders = options.directories && !options.files;
    let identifier = if folders { "public.folder" } else { "public.item" };
    // SAFETY: UIKit and UniformTypeIdentifiers calls on the main thread.
    let picker = unsafe {
        let (Some(types), Some(picker_class)) = (
            AnyClass::get(c"UTType"),
            AnyClass::get(c"UIDocumentPickerViewController"),
        ) else {
            let _ = reply.send(Err(anyhow!("The document picker isn't available")));
            return;
        };
        let content_type: Option<Retained<AnyObject>> =
            msg_send![types, typeWithIdentifier: &*NSString::from_str(identifier)];
        let Some(content_type) = content_type else {
            let _ = reply.send(Err(anyhow!("No content type {identifier}")));
            return;
        };
        let content_types = NSArray::from_retained_slice(&[content_type]);
        let picker: Allocated<AnyObject> = msg_send![picker_class, alloc];
        let picker: Option<Retained<AnyObject>> = msg_send![
            picker,
            initForOpeningContentTypes: &*content_types,
            asCopy: !folders
        ];
        let Some(picker) = picker else {
            let _ = reply.send(Err(anyhow!("The document picker didn't open")));
            return;
        };
        let _: () = msg_send![&*picker, setAllowsMultipleSelection: options.multiple];
        picker
    };
    let delegate = PickerDelegate::alloc(main_thread).set_ivars(DelegateIvars {
        reply: RefCell::new(Some(reply)),
        in_place: folders,
    });
    let delegate: Retained<PickerDelegate> = unsafe { msg_send![super(delegate), init] };
    // SAFETY: UIKit calls on the main thread.
    unsafe {
        let _: () = msg_send![&*picker, setDelegate: &*delegate];
        let _: () = msg_send![
            controller,
            presentViewController: &*picker,
            animated: true,
            completion: Option::<&block2::Block<dyn Fn()>>::None
        ];
    }
    // A picker still open answers None when another replaces it.
    if let Some(previous) = CURRENT.with(|current| current.borrow_mut().replace(delegate)) {
        if let Some(reply) = previous.ivars().reply.borrow_mut().take() {
            let _ = reply.send(Ok(None));
        }
    }
}

/// Where a new file named `suggested_name` goes: the app's Documents
/// folder, under a name no file there has yet.
pub(super) fn new_path(suggested_name: Option<&str>) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("No home folder"))?;
    let documents = Path::new(&home).join("Documents");
    std::fs::create_dir_all(&documents)?;
    let name = suggested_name
        .map(|name| name.trim_matches('/'))
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .unwrap_or("Download");
    let candidate = documents.join(name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    (2..)
        .map(|number| {
            documents.join(match extension {
                Some(extension) => format!("{stem} {number}.{extension}"),
                None => format!("{stem} {number}"),
            })
        })
        .find(|path| !path.exists())
        .ok_or_else(|| anyhow!("No free name for {name}"))
}
