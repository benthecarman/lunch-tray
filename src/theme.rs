//! System light/dark detection through the XDG desktop portal, with a
//! `gsettings` fallback. Winit does not report the theme on Wayland.

use std::sync::mpsc::Sender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemTheme {
    Light,
    Dark,
}

const PORTAL_DEST: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const PORTAL_IFACE: &str = "org.freedesktop.portal.Settings";

fn color_scheme_to_theme(v: u32) -> SystemTheme {
    match v {
        1 => SystemTheme::Dark,
        _ => SystemTheme::Light,
    }
}

fn read_portal(proxy: &zbus::blocking::Proxy<'_>) -> Option<SystemTheme> {
    let value: zbus::zvariant::OwnedValue = proxy
        .call("ReadOne", &("org.freedesktop.appearance", "color-scheme"))
        .or_else(|_| {
            proxy
                .call::<_, _, zbus::zvariant::OwnedValue>(
                    "Read",
                    &("org.freedesktop.appearance", "color-scheme"),
                )
                .and_then(unwrap_variant)
        })
        .ok()?;
    let n = u32::try_from(value).ok()?;
    Some(color_scheme_to_theme(n))
}

fn unwrap_variant(v: zbus::zvariant::OwnedValue) -> zbus::Result<zbus::zvariant::OwnedValue> {
    let inner: zbus::zvariant::Value<'_> = v.into();
    match inner {
        zbus::zvariant::Value::Value(b) => Ok((*b).try_into()?),
        other => Ok(other.try_into()?),
    }
}

fn read_gsettings() -> Option<SystemTheme> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    if s.contains("prefer-dark") {
        Some(SystemTheme::Dark)
    } else {
        Some(SystemTheme::Light)
    }
}

/// Read the current system theme once.
pub fn current() -> SystemTheme {
    let via_portal = zbus::blocking::Connection::session().ok().and_then(|conn| {
        let proxy =
            zbus::blocking::Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, PORTAL_IFACE).ok()?;
        read_portal(&proxy)
    });
    via_portal
        .or_else(read_gsettings)
        .unwrap_or(SystemTheme::Dark)
}

/// Send the current theme, then every change, on `tx`. Returns when the
/// receiver is dropped or the D-Bus connection ends.
pub fn watch(tx: Sender<SystemTheme>) {
    let _ = tx.send(current());
    let Ok(conn) = zbus::blocking::Connection::session() else {
        return;
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(&conn, PORTAL_DEST, PORTAL_PATH, PORTAL_IFACE)
    else {
        return;
    };
    let Ok(signals) = proxy.receive_signal("SettingChanged") else {
        return;
    };
    for msg in signals {
        let body = msg.body();
        let Ok((ns, key, value)) =
            body.deserialize::<(String, String, zbus::zvariant::Value<'_>)>()
        else {
            continue;
        };
        if ns != "org.freedesktop.appearance" || key != "color-scheme" {
            continue;
        }
        let n: Option<u32> = match value {
            zbus::zvariant::Value::U32(n) => Some(n),
            zbus::zvariant::Value::Value(inner) => match *inner {
                zbus::zvariant::Value::U32(n) => Some(n),
                _ => None,
            },
            _ => None,
        };
        if let Some(n) = n
            && tx.send(color_scheme_to_theme(n)).is_err()
        {
            return;
        }
    }
}
