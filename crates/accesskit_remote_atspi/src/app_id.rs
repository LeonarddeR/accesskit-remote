//! Resolves a process to its desktop-file-style application id by finding a
//! well-known session-bus name it owns (a GApplication owns its application
//! id as a bus name). Results are cached per pid; every failure degrades to
//! `None` so discovery never blocks on the session bus.

use atspi::zbus::fdo::DBusProxy;
use atspi::zbus::Connection;
use std::collections::HashMap;

/// Pid → application-id resolver over the session bus, with a lazily opened
/// connection and a per-pid result cache (`None` results included, so an app
/// without a bus name is swept only once).
#[derive(Default)]
pub struct AppIdResolver {
    session: Option<Connection>,
    session_failed: bool,
    cache: HashMap<u32, Option<String>>,
}

impl AppIdResolver {
    /// The application id for `pid`, from the cache or a session-bus sweep.
    pub async fn app_id_for_pid(&mut self, pid: u32) -> Option<String> {
        if let Some(cached) = self.cache.get(&pid) {
            return cached.clone();
        }
        let resolved = self.resolve(pid).await;
        self.cache.insert(pid, resolved.clone());
        resolved
    }

    /// Sweeps the session bus: every candidate well-known name whose owner
    /// reports `pid`, reduced to one deterministic pick.
    async fn resolve(&mut self, pid: u32) -> Option<String> {
        let conn = self.session().await?;
        let dbus = DBusProxy::new(conn).await.ok()?;
        let names = dbus.list_names().await.ok()?;
        let mut candidates = Vec::new();
        for name in names {
            if !is_candidate_name(name.as_str()) {
                continue;
            }
            let label = name.as_str().to_owned();
            let owner = dbus.get_connection_unix_process_id(name.into()).await.ok();
            if owner == Some(pid) {
                candidates.push(label);
            }
        }
        pick_app_id(candidates)
    }

    /// The lazily opened session-bus connection; stays `None` after a failed
    /// open so the sweep is not retried on every discovery.
    async fn session(&mut self) -> Option<&Connection> {
        if self.session.is_none() && !self.session_failed {
            match Connection::session().await {
                Ok(conn) => self.session = Some(conn),
                Err(_) => self.session_failed = true,
            }
        }
        self.session.as_ref()
    }
}

/// Whether a bus name can be an application id: well known (not a `:`-prefixed
/// unique name), reverse-DNS with at least two dots, and not under an
/// infrastructure prefix that desktop processes own alongside their own id.
fn is_candidate_name(name: &str) -> bool {
    if name.starts_with(':') {
        return false;
    }
    const INFRASTRUCTURE: [&str; 3] = ["org.freedesktop.", "org.a11y.", "org.gtk."];
    if INFRASTRUCTURE.iter().any(|prefix| name.starts_with(prefix)) {
        return false;
    }
    name.bytes().filter(|&b| b == b'.').count() >= 2
}

/// The deterministic pick among the candidate names one pid owns: the
/// lexicographically first.
fn pick_app_id(mut candidates: Vec<String>) -> Option<String> {
    candidates.sort();
    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_rejects_unique_names() {
        assert!(!is_candidate_name(":1.42"));
    }

    #[test]
    fn candidate_rejects_infrastructure_prefixes() {
        assert!(!is_candidate_name("org.freedesktop.FileManager1"));
        assert!(!is_candidate_name("org.a11y.Bus"));
        assert!(!is_candidate_name("org.gtk.vfs.Daemon"));
    }

    #[test]
    fn candidate_requires_reverse_dns() {
        assert!(!is_candidate_name("Notifications"));
        assert!(!is_candidate_name("org.gnome"));
        assert!(is_candidate_name("org.gnome.TextEditor"));
        assert!(is_candidate_name("org.libreoffice.LibreOffice"));
    }

    #[test]
    fn pick_is_deterministic_and_sorted() {
        assert_eq!(
            pick_app_id(vec!["org.gnome.b".into(), "org.gnome.a".into()]),
            Some("org.gnome.a".to_owned())
        );
        assert_eq!(pick_app_id(Vec::new()), None);
    }
}
