//! Executes `Effect::Transfer` by forwarding commands to the engine, starts
//! the engine, relays engine events as `BgEvent::Transfer`, and holds the
//! production `TarTransfer` adapter.
//!
//! # Behavior
//!
//! - `start_engine` creates the engine channels (events bounded at 256),
//!   spawns `transfer::engine::run` with `EngineConfig::from_settings`, the
//!   given connections and partial store, and `Arc::new(TarStreams)`, plus a
//!   relay task that sends every `TransferEvent` as `BgEvent::Transfer`
//!   until either side closes. The returned sender is `Ctx::engine`;
//!   dropping every clone stops the engine.
//! - `send` forwards one command; a closed engine only happens during
//!   shutdown, so the command is dropped with a comment saying so.
//! - `TarStreams` implements `transfer::engine::TarTransfer` by calling
//!   `transfer::tar::upload` / `transfer::tar::download` with the same
//!   arguments.
//!
//! # Public API (Wave 3, ui/effects/transfer)
//!
//! ```text
//! pub fn start_engine(
//!     settings: &Settings,
//!     connections: Arc<dyn Connections>,
//!     partials: Arc<dyn PartialStore>,
//!     events: UnboundedSender<BgEvent>,
//! ) -> TransferCommandTx;
//! pub fn send(ctx: &Ctx, command: TransferCommand);
//! pub struct TarStreams;
//! impl TarTransfer for TarStreams { /* delegates to transfer::tar */ }
//! ```
