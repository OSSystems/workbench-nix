//! workbench's web UI: a Leptos app the server renders and the browser
//! hydrates. [`app`] is the pages, [`api`] the server functions they call;
//! the server side, [`server`], reads and controls the host through the
//! workbench crate, as the `workbench-web` user.

#![recursion_limit = "512"]
// Components return views, which are used whether marked or not.
#![allow(clippy::must_use_candidate)]

pub mod api;
pub mod app;
#[cfg(feature = "ssr")]
pub mod server;
mod term;

/// The browser's entry point, called by Leptos's hydration script.
#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(app::App);
}
