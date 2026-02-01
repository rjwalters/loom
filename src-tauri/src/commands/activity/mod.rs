// Activity command submodules organized by domain

mod budget;
mod costs;
mod db;
mod github_events;
mod logging;
mod metrics;
mod patterns;
mod recommendations;
mod timeline;
mod tokens;
mod velocity;

// Re-export all Tauri commands and their associated types.
// Some exports are consumed only by Tauri's generate_handler! macro or
// reserved for future IPC use, so suppress unused-import warnings here.
#[allow(unused_imports)]
pub use budget::*;
#[allow(unused_imports)]
pub use costs::*;
#[allow(unused_imports)]
pub use github_events::*;
#[allow(unused_imports)]
pub use logging::*;
#[allow(unused_imports)]
pub use metrics::*;
#[allow(unused_imports)]
pub use patterns::*;
#[allow(unused_imports)]
pub use recommendations::*;
#[allow(unused_imports)]
pub use timeline::*;
#[allow(unused_imports)]
pub use tokens::*;
#[allow(unused_imports)]
pub use velocity::*;
