//! Bring our own window to the front on Wayland.
//!
//! A compositor only raises a window on request when the request carries an
//! XDG activation token that traces back to a user action. The tray host
//! hands us one before each click. Neither egui nor winit apply a token to an
//! existing window, so this speaks the protocol directly on the window's
//! surface, the same thing GTK does for `gtk_window_set_startup_id`.

use anyhow::{Context, Result, bail};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore XdgActivationV1);

/// Ask the compositor to focus and raise `window` using `token`.
pub fn activate(window: &winit::window::Window, token: &str) -> Result<()> {
    let RawDisplayHandle::Wayland(display) = window.display_handle()?.as_raw() else {
        bail!("not a Wayland window");
    };
    let RawWindowHandle::Wayland(handle) = window.window_handle()?.as_raw() else {
        bail!("not a Wayland window");
    };
    // SAFETY: both pointers belong to winit's live window and connection.
    // The backend is marked foreign, so dropping it leaves the display open.
    let backend = unsafe {
        wayland_backend::client::Backend::from_foreign_display(display.display.as_ptr().cast())
    };
    let conn = Connection::from_backend(backend);
    let surface_id = unsafe {
        wayland_backend::client::ObjectId::from_ptr(
            WlSurface::interface(),
            handle.surface.as_ptr().cast(),
        )
    }
    .context("surface id")?;
    let surface = WlSurface::from_id(&conn, surface_id).context("surface proxy")?;
    let (globals, queue) = registry_queue_init::<State>(&conn).context("wayland registry")?;
    let activation: XdgActivationV1 = globals
        .bind(&queue.handle(), 1..=1, ())
        .context("the compositor does not offer xdg_activation_v1")?;
    activation.activate(token.to_string(), &surface);
    conn.flush().context("flush")?;
    Ok(())
}
