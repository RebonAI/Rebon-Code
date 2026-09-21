//! The `config-options` seat: every row of the settings panel, registered.
//!
//! `rebon-types` says what a [`ConfigOption`] *is*; this crate is where one is
//! *registered*, by whichever plugin owns the thing it configures, together
//! with a [`ConfigOptionProvider`] saying what the value is right now and what
//! applying a new one means. Registration is an effect of the registering
//! context — dispose it and the row is gone, which is what makes a plugin's
//! settings disappear with the plugin instead of lingering as a row that
//! changes nothing.
//!
//! Every surface reads the seat. A fixed list somewhere would mean one
//! surface's crate deciding what every other surface shows, and a plugin with
//! no way to add a row at all.
//!
//! Two things a fixed list could not do, and the reason the provider is a
//! trait rather than a value:
//!
//! - **The value is read, not stored.** A row shows what is in force at the
//!   moment the panel is drawn, so something changed by a slash command or by
//!   editing the config file is already right — there is no copy on the row to
//!   go stale.
//! - **The choices are asked for, not fixed.** The models a router can pick
//!   from depend on the provider in force, so a list baked in at registration
//!   would be wrong as soon as the provider changed.
//!
//! No behaviour lives here. The seat holds specs and providers; applying is
//! the provider's own business, and the seat only says whether it worked.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rebon_kernel::{Context, Disposer, KernelError, Service};
pub use rebon_types::{ConfigOption, ConfigOptionType, ConfigOptionValue};

/// The seat's service name, for the `provides` / `inject` lists a
/// [`rebon_kernel::PluginMeta`] declares.
pub const CONFIG_SEAT_SERVICE: &str = "config-options";

/// Service marker for the seat.
pub struct ConfigSeatService;

impl Service for ConfigSeatService {
    type Interface = ConfigSeat;
    const NAME: &'static str = CONFIG_SEAT_SERVICE;
}

/// What a row is, as against what it currently reads.
///
/// Everything here is fixed at registration. The value and the choices are
/// not: they come from the [`ConfigOptionProvider`] every time the panel asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigOptionSpec {
    /// The id an apply action carries. Stable: it is what a front end's own
    /// dispatch, a saved preference and a test all name.
    pub id: String,
    /// The option's name, shown in the list and above the detail.
    pub name: String,
    /// What it does, shown under the name.
    pub description: Option<String>,
    /// Groups rows in the panel.
    pub category: Option<String>,
    /// Whether it is edited by typing or by cycling values.
    pub option_type: ConfigOptionType,
}

impl ConfigOptionSpec {
    /// A cycling option. `choices` come from the provider.
    pub fn select(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Select,
        }
    }

    /// A free-text option.
    pub fn text(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Text,
        }
    }

    /// What the row says it does.
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Which group the row sits in.
    pub fn in_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }
}

/// What the owner of a setting can answer about it.
///
/// The panel asks on every frame, so all three are cheap reads of state that
/// already exists. None of them may block on a model call or a network round
/// trip.
///
/// Every method takes the session being asked about, because some settings
/// have no single answer. `--acp` and `serve` host many sessions in one
/// process: the permission mode and the effort level are that session's, while
/// the response language and the update preference are the machine's. A
/// process-wide setting ignores the argument; a session-scoped one must not.
/// `None` means there is no session in hand — a panel opened before one is
/// built — and a session-scoped provider answers its startup default.
pub trait ConfigOptionProvider: Send + Sync {
    /// The value in force right now.
    fn current(&self, session: Option<&str>) -> String;

    /// The values this option cycles through, right now.
    ///
    /// Empty means nothing to cycle, which is what a free-text option always
    /// answers. The default is empty so a text option does not have to say so
    /// twice.
    fn choices(&self, session: Option<&str>) -> Vec<ConfigOptionValue> {
        let _ = session;
        Vec::new()
    }

    /// Put `value` in force and persist it.
    ///
    /// The `Err` string is shown to the user, so it says what went wrong and
    /// what to do — not a type name. Returning `Ok` when nothing was persisted
    /// is the one thing this must not do: the panel redraws from `current`,
    /// so a silent no-op shows as a value that snaps back.
    fn apply(&self, session: Option<&str>, value: &str) -> Result<(), String>;
}

/// A provider for a row that is read but never set here.
///
/// The `model` row is the case: the panel shows what the session resolved,
/// and changing it goes through the model picker, which knows about providers
/// and profiles. A row like that is still a row — it is how the panel reports
/// the value — but applying to it is refused rather than half-done.
pub struct ReadOnlyOption<F> {
    read: F,
    refusal: String,
}

impl<F> ReadOnlyOption<F>
where
    F: Fn() -> String + Send + Sync,
{
    /// `refusal` is what the user is told when they try to change it, so it
    /// names where the change is actually made.
    pub fn new(read: F, refusal: impl Into<String>) -> Self {
        Self {
            read,
            refusal: refusal.into(),
        }
    }
}

impl<F> ConfigOptionProvider for ReadOnlyOption<F>
where
    F: Fn() -> String + Send + Sync,
{
    fn current(&self, _session: Option<&str>) -> String {
        (self.read)()
    }

    fn apply(&self, _session: Option<&str>, _value: &str) -> Result<(), String> {
        Err(self.refusal.clone())
    }
}

struct Entry {
    token: u64,
    spec: ConfigOptionSpec,
    provider: Arc<dyn ConfigOptionProvider>,
    owner: String,
}

/// The registry behind the settings panel's Config tab.
#[derive(Default)]
pub struct ConfigSeat {
    entries: RwLock<Vec<Entry>>,
    next_token: AtomicU64,
}

impl ConfigSeat {
    /// The seat's service name.
    pub const NAME: &'static str = ConfigSeatService::NAME;

    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register an option on `ctx`. The registration is an effect of the
    /// context: disposing it removes the row.
    ///
    /// Refused with [`KernelError::DuplicateProvider`] when the id is already
    /// registered. Two rows under one id would mean whichever registered first
    /// wins an apply, silently, and the front ends dispatch by id.
    pub fn register(
        self: &Arc<Self>,
        ctx: &Context,
        spec: ConfigOptionSpec,
        provider: Arc<dyn ConfigOptionProvider>,
    ) -> Result<(), KernelError> {
        let id = spec.id.trim().to_string();
        if id.is_empty() {
            return Err(KernelError::Other(
                "config-options: an option needs an id".to_string(),
            ));
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let owner = ctx.label().to_string();
        {
            let mut entries = self.entries.write().unwrap();
            if let Some(taken) = entries.iter().find(|entry| entry.spec.id == id) {
                return Err(KernelError::DuplicateProvider {
                    plugin: owner,
                    service: format!("{}:{id} (already held by {})", Self::NAME, taken.owner),
                });
            }
            entries.push(Entry {
                token,
                spec: ConfigOptionSpec {
                    id: id.clone(),
                    ..spec
                },
                provider,
                owner,
            });
        }
        let weak = Arc::downgrade(self);
        ctx.effect_labeled(&format!("config-option({id})"), || {
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

    /// Every registered row, with its value and choices read now.
    ///
    /// Registration order, which is the order the panel shows: whoever
    /// registered first is first, so the built-in rows keep their order and a
    /// plugin's rows arrive under them.
    pub fn options(&self, session: Option<&str>) -> Vec<ConfigOption> {
        self.entries
            .read()
            .unwrap()
            .iter()
            .map(|entry| ConfigOption {
                id: entry.spec.id.clone(),
                name: entry.spec.name.clone(),
                description: entry.spec.description.clone(),
                category: entry.spec.category.clone(),
                option_type: entry.spec.option_type,
                current_value: entry.provider.current(session),
                options: entry.provider.choices(session),
            })
            .collect()
    }

    /// Whether an id is registered at all.
    pub fn has(&self, id: &str) -> bool {
        self.entries
            .read()
            .unwrap()
            .iter()
            .any(|entry| entry.spec.id == id)
    }

    /// Apply a value to one row.
    ///
    /// An unregistered id is an `Err` rather than a silent no-op: it means a
    /// front end dispatched on an id nothing owns, which the user would
    /// otherwise see as a setting that refuses to change for no reason.
    ///
    /// The provider is called without the lock held. Applying persists, and a
    /// persist that decided to re-read the seat under a held write lock would
    /// deadlock a settings write against the panel drawing behind it.
    pub fn apply(&self, session: Option<&str>, id: &str, value: &str) -> Result<(), String> {
        let provider = {
            let entries = self.entries.read().unwrap();
            entries
                .iter()
                .find(|entry| entry.spec.id == id)
                .map(|entry| Arc::clone(&entry.provider))
        };
        match provider {
            Some(provider) => provider.apply(session, value),
            None => Err(format!("no setting is registered under `{id}`")),
        }
    }

    /// How many rows are registered.
    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The seat the process kernel provides, if one has booted.
///
/// `None` before the assembly layer boots a kernel, and while the plugin that
/// provides the seat is being reloaded. Both mean the same thing to a caller:
/// nothing is registered right now, so the panel has no rows to show.
pub fn process_config_seat() -> Option<Arc<ConfigSeat>> {
    rebon_kernel::process_kernel()?
        .context()
        .get::<ConfigSeatService>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Cell {
        value: Mutex<String>,
        choices: Vec<&'static str>,
        refuse: bool,
    }

    impl Cell {
        fn new(value: &str, choices: &[&'static str]) -> Arc<Self> {
            Arc::new(Self {
                value: Mutex::new(value.to_string()),
                choices: choices.to_vec(),
                refuse: false,
            })
        }
    }

    impl ConfigOptionProvider for Cell {
        fn current(&self, _session: Option<&str>) -> String {
            self.value.lock().unwrap().clone()
        }

        fn choices(&self, _session: Option<&str>) -> Vec<ConfigOptionValue> {
            self.choices
                .iter()
                .map(|value| ConfigOptionValue {
                    value: value.to_string(),
                    name: value.to_uppercase(),
                    description: None,
                })
                .collect()
        }

        fn apply(&self, _session: Option<&str>, value: &str) -> Result<(), String> {
            if self.refuse {
                return Err("nope".to_string());
            }
            *self.value.lock().unwrap() = value.to_string();
            Ok(())
        }
    }

    fn kernel() -> Arc<rebon_kernel::Kernel> {
        rebon_kernel::Kernel::new()
    }

    #[test]
    fn a_registered_row_reports_the_value_its_provider_reads_now() {
        let k = kernel();
        let seat = ConfigSeat::new();
        let cell = Cell::new("off", &["on", "off"]);
        seat.register(
            k.context(),
            ConfigOptionSpec::select("thing", "Thing").describe("does a thing"),
            cell.clone(),
        )
        .expect("registers");

        let options = seat.options(None);
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, "thing");
        assert_eq!(options[0].current_value, "off");
        assert_eq!(options[0].description.as_deref(), Some("does a thing"));
        assert_eq!(
            options[0]
                .options
                .iter()
                .map(|value| value.value.as_str())
                .collect::<Vec<_>>(),
            vec!["on", "off"]
        );

        // Changed behind the panel's back: the next read is already right,
        // because the row holds no copy.
        *cell.value.lock().unwrap() = "on".into();
        assert_eq!(seat.options(None)[0].current_value, "on");
    }

    #[test]
    fn applying_goes_to_the_provider_and_an_unknown_id_is_refused() {
        let k = kernel();
        let seat = ConfigSeat::new();
        let cell = Cell::new("off", &["on", "off"]);
        seat.register(
            k.context(),
            ConfigOptionSpec::select("thing", "Thing"),
            cell.clone(),
        )
        .expect("registers");

        seat.apply(None, "thing", "on").expect("applies");
        assert_eq!(cell.current(None), "on");

        let err = seat
            .apply(None, "nothing", "on")
            .expect_err("unknown id refused");
        assert!(err.contains("nothing"), "{err}");
    }

    #[test]
    fn one_id_may_only_be_registered_once() {
        let k = kernel();
        let seat = ConfigSeat::new();
        seat.register(
            k.context(),
            ConfigOptionSpec::select("thing", "Thing"),
            Cell::new("off", &[]),
        )
        .expect("first registers");
        let err = seat
            .register(
                k.context(),
                ConfigOptionSpec::select("thing", "Other"),
                Cell::new("on", &[]),
            )
            .expect_err("second is refused");
        assert!(
            matches!(err, KernelError::DuplicateProvider { .. }),
            "{err:?}"
        );
        assert_eq!(seat.len(), 1);
    }

    #[test]
    fn an_empty_id_is_refused() {
        let k = kernel();
        let seat = ConfigSeat::new();
        let err = seat
            .register(
                k.context(),
                ConfigOptionSpec::select("  ", "Thing"),
                Cell::new("off", &[]),
            )
            .expect_err("refused");
        assert!(matches!(err, KernelError::Other(_)), "{err:?}");
    }

    #[test]
    fn disposing_the_registering_context_takes_the_row_with_it() {
        let k = kernel();
        let seat = ConfigSeat::new();
        let scope = k.context().fork("plugin");
        seat.register(
            &scope,
            ConfigOptionSpec::select("thing", "Thing"),
            Cell::new("off", &[]),
        )
        .expect("registers");
        assert!(seat.has("thing"));

        scope.dispose();
        assert!(
            !seat.has("thing"),
            "a row must not outlive the context that registered it"
        );
        assert!(seat.is_empty());
    }

    #[test]
    fn a_read_only_row_reports_a_value_and_refuses_a_change() {
        let k = kernel();
        let seat = ConfigSeat::new();
        seat.register(
            k.context(),
            ConfigOptionSpec::text("model", "Model"),
            Arc::new(ReadOnlyOption::new(
                || "claude-opus-5".to_string(),
                "change the model with /model",
            )),
        )
        .expect("registers");

        assert_eq!(seat.options(None)[0].current_value, "claude-opus-5");
        let err = seat.apply(None, "model", "gpt-5").expect_err("refused");
        assert_eq!(err, "change the model with /model");
    }
}
