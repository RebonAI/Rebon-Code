//! The `ui-registry` seat: every dialog a surface can open, registered.
//!
//! `rebon-dialog` says what a dialog *is* — a keyboard reducer plus a
//! declarative view. This crate is where one is *registered*, by whichever
//! plugin owns it, as a factory that builds the model on demand. The
//! built-in panels are registered by the front end that owns their state;
//! a plugin registers its own the same way, on its own [`Context`], and the
//! registration is an effect — dispose the context and the dialog is gone,
//! which is what makes a plugin unloadable without a front end knowing it
//! existed.
//!
//! A front end opens a panel by id with [`UiSeat::open`] and pushes the
//! model onto its dialog stack. It never names a dialog's type, so the set
//! of panels is data rather than a `match` every surface has to keep in
//! step.
//!
//! No painting lives here, and no terminal type. The seat holds factories;
//! how a [`ViewSpec`](rebon_dialog::model::ViewSpec) becomes pixels is the
//! surface's business.

#![forbid(unsafe_code)]

pub mod ids;
pub mod input;

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_dialog::model::DialogModel;
use rebon_kernel::{Context, Disposer, KernelError, Service};

/// Service definition of the seat. `Interface` is the seat itself:
/// consumers look the seat service up on the kernel context and get the
/// registry.
pub struct UiSeatService;

impl Service for UiSeatService {
    type Interface = UiSeat;
    const NAME: &'static str = "ui-registry";
}

/// What opening a dialog is given.
///
/// `values` is the portable half: positional strings, which is all a
/// plugin's dialog ever needs and all that could cross a process boundary.
/// `payload` is the escape hatch for a built-in whose inputs are richer
/// than strings — a diagnostics report, a settings projection. The seat
/// never looks inside it; the factory that registered the dialog is the
/// only thing that knows the type, and downcasts it back.
#[derive(Clone, Default)]
pub struct DialogArgs {
    /// Positional string inputs.
    pub values: Vec<String>,
    /// An owned, surface-defined payload the factory downcasts.
    pub payload: Option<Arc<dyn Any + Send + Sync>>,
}

impl DialogArgs {
    /// No inputs at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Positional string inputs.
    pub fn values<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            values: values.into_iter().map(Into::into).collect(),
            payload: None,
        }
    }

    /// A rich payload the registering surface will downcast.
    pub fn payload<T: Any + Send + Sync>(payload: T) -> Self {
        Self {
            values: Vec::new(),
            payload: Some(Arc::new(payload)),
        }
    }

    /// The input at `index`, or `""` when there is none.
    pub fn value_at(&self, index: usize) -> &str {
        self.values.get(index).map_or("", String::as_str)
    }

    /// The payload, when it is a `T`.
    pub fn payload_as<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.payload.as_ref()?.downcast_ref::<T>()
    }
}

impl std::fmt::Debug for DialogArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogArgs")
            .field("values", &self.values)
            .field("payload", &self.payload.as_ref().map(|_| "<opaque>"))
            .finish()
    }
}

/// Builds a dialog's model. Returns `None` when the inputs do not name a
/// panel worth opening — no active provider for the model picker, no
/// loaded skills for the skills selector — which is the caller's cue to
/// fall back to whatever it did before the panel existed.
pub type DialogFactory = Arc<dyn Fn(DialogArgs) -> Option<Box<dyn DialogModel>> + Send + Sync>;

/// One dialog a surface can open.
#[derive(Clone)]
pub struct DialogDef {
    /// Stable id, the same one the model reports from `DialogModel::id`.
    pub id: &'static str,
    /// Builds the model.
    pub factory: DialogFactory,
}

impl DialogDef {
    /// A dialog built by `factory`.
    pub fn new(
        id: &'static str,
        factory: impl Fn(DialogArgs) -> Option<Box<dyn DialogModel>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id,
            factory: Arc::new(factory),
        }
    }
}

impl std::fmt::Debug for DialogDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogDef").field("id", &self.id).finish()
    }
}

struct Entry {
    token: u64,
    def: DialogDef,
    owner: String,
}

/// The registry behind the `ui-registry` service.
pub struct UiSeat {
    entries: RwLock<Vec<Entry>>,
    next_token: AtomicU64,
}

impl Default for UiSeat {
    fn default() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            next_token: AtomicU64::new(0),
        }
    }
}

impl UiSeat {
    /// The seat's service name.
    pub const NAME: &'static str = UiSeatService::NAME;

    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a dialog on `ctx`. The registration is an effect of the
    /// context: disposing it unregisters the dialog.
    ///
    /// Refused with [`KernelError::DuplicateProvider`] when the id is
    /// already registered. Two panels answering to one id would mean
    /// whichever registered first wins, silently.
    pub fn register_dialog(
        self: &Arc<Self>,
        ctx: &Context,
        def: DialogDef,
    ) -> Result<(), KernelError> {
        let id = def.id;
        if id.trim().is_empty() {
            return Err(KernelError::Other(
                "ui-registry: a dialog needs an id".to_string(),
            ));
        }
        let owner = ctx.label().to_string();
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        {
            let mut entries = self.entries.write().unwrap();
            if let Some(taken) = entries.iter().find(|entry| entry.def.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: owner,
                    service: format!("{}:{id} (already owned by {})", Self::NAME, taken.owner),
                });
            }
            entries.push(Entry { token, def, owner });
        }
        let weak = Arc::downgrade(self);
        ctx.effect_labeled(&format!("dialog({id})"), || {
            Disposer::new(move || {
                if let Some(seat) = weak.upgrade() {
                    seat.entries
                        .write()
                        .unwrap()
                        .retain(|entry| entry.token != token);
                }
            })
        });
        Ok(())
    }

    /// Build the dialog registered as `id`. `None` when nothing is
    /// registered under that id, or when the factory declined.
    pub fn open(&self, id: &str, args: DialogArgs) -> Option<Box<dyn DialogModel>> {
        let factory = {
            let entries = self.entries.read().unwrap();
            entries
                .iter()
                .find(|entry| entry.def.id == id)
                .map(|entry| entry.def.factory.clone())?
        };
        factory(args)
    }

    /// Whether a dialog is registered under `id`.
    pub fn has(&self, id: &str) -> bool {
        self.entries
            .read()
            .unwrap()
            .iter()
            .any(|entry| entry.def.id == id)
    }

    /// Every registered dialog id, in registration order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .map(|entry| entry.def.id)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rebon_dialog::model::{DialogOutcome, KeyPress, ListView, ViewSpec};
    use rebon_kernel::Kernel;

    #[derive(Clone)]
    struct Probe(&'static str);

    impl DialogModel for Probe {
        rebon_dialog::dialog_plumbing!();

        fn id(&self) -> &'static str {
            self.0
        }

        fn on_key(&mut self, _press: KeyPress) -> DialogOutcome {
            DialogOutcome::None
        }

        fn view(&self) -> ViewSpec {
            ViewSpec::List(ListView::default())
        }
    }

    #[test]
    fn a_registered_dialog_opens_by_id_and_an_unknown_one_does_not() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("test");
        let seat = UiSeat::new();
        seat.register_dialog(
            &ctx,
            DialogDef::new("probe", |_| Some(Box::new(Probe("probe")))),
        )
        .unwrap();

        assert!(seat.has("probe"));
        assert_eq!(seat.ids(), vec!["probe"]);
        assert_eq!(
            seat.open("probe", DialogArgs::none()).map(|d| d.id()),
            Some("probe")
        );
        assert!(seat.open("missing", DialogArgs::none()).is_none());
    }

    #[test]
    fn a_factory_may_decline_to_open() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("test");
        let seat = UiSeat::new();
        seat.register_dialog(
            &ctx,
            DialogDef::new("picky", |args| {
                (!args.values.is_empty()).then(|| Box::new(Probe("picky")) as Box<dyn DialogModel>)
            }),
        )
        .unwrap();

        assert!(seat.open("picky", DialogArgs::none()).is_none());
        assert!(seat.open("picky", DialogArgs::values(["x"])).is_some());
    }

    #[test]
    fn args_carry_positional_values_and_an_opaque_payload() {
        let args = DialogArgs::values(["a", "b"]);
        assert_eq!(args.value_at(0), "a");
        assert_eq!(args.value_at(1), "b");
        assert_eq!(args.value_at(2), "");
        assert!(args.payload_as::<u32>().is_none());

        let args = DialogArgs::payload(7u32);
        assert_eq!(args.payload_as::<u32>(), Some(&7));
        assert!(args.payload_as::<String>().is_none(), "wrong type declines");
    }

    #[test]
    fn one_id_cannot_be_registered_twice() {
        let kernel = Kernel::new();
        let ctx = kernel.context().fork("test");
        let seat = UiSeat::new();
        seat.register_dialog(
            &ctx,
            DialogDef::new("dup", |_| Some(Box::new(Probe("dup")))),
        )
        .unwrap();
        let second = seat.register_dialog(
            &ctx,
            DialogDef::new("dup", |_| Some(Box::new(Probe("dup")))),
        );
        assert!(matches!(second, Err(KernelError::DuplicateProvider { .. })));
        assert_eq!(seat.len(), 1);
    }

    #[test]
    fn disposing_the_context_unregisters_its_dialogs() {
        let kernel = Kernel::new();
        let seat = UiSeat::new();
        {
            let ctx = kernel.context().fork("plugin");
            seat.register_dialog(
                &ctx,
                DialogDef::new("gone", |_| Some(Box::new(Probe("gone")))),
            )
            .unwrap();
            assert!(seat.has("gone"));
            ctx.dispose();
        }
        assert!(!seat.has("gone"), "the effect should have unregistered it");
        assert!(seat.is_empty());
    }
}
