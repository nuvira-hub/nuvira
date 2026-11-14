//! GNOME's monitor configuration (`org.gnome.Mutter.DisplayConfig` on the
//! session bus), to change a monitor's mode on Wayland, where no program can do
//! it by itself. A blocking API: the calls run on a small runtime of their own,
//! in a thread of their own (the callers are blocking tasks of the agent).
//!
//! The configuration is applied as *temporary* (method 1): GNOME does not save
//! it, and the original one is applied again by the agent when the session ends.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ashpd::zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use ashpd::zbus::{Connection, Proxy};

const DEST: &str = "org.gnome.Mutter.DisplayConfig";
/// `ApplyMonitorsConfig` method: not saved by GNOME.
const TEMPORARY: u32 = 1;

/// A mode a monitor can take.
#[derive(Debug, Clone)]
pub struct Mode {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub refresh: f64,
    pub scales: Vec<f64>,
}

/// A monitor turned on: its modes, the current one, and its logical monitor.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub connector: String,
    pub modes: Vec<Mode>,
    pub current: String,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
}

impl Monitor {
    pub fn mode(&self, id: &str) -> Option<&Mode> {
        self.modes.iter().find(|m| m.id == id)
    }

    /// Logical size with `mode` (pixels over the scale, swapped when rotated).
    fn logical(&self, mode: &Mode) -> (i32, i32) {
        let (w, h) = ((f64::from(mode.width) / self.scale) as i32, (f64::from(mode.height) / self.scale) as i32);
        if self.transform % 2 == 1 { (h, w) } else { (w, h) }
    }
}

type Spec = (String, String, String, String);
type RawMode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type RawMonitor = (Spec, Vec<RawMode>, HashMap<String, OwnedValue>);
type RawLogical = (i32, i32, f64, u32, bool, Vec<Spec>, HashMap<String, OwnedValue>);
type State = (u32, Vec<RawMonitor>, Vec<RawLogical>, HashMap<String, OwnedValue>);

fn blocking<T: Send + 'static>(f: impl AsyncFnOnce(Connection) -> Result<T> + Send + 'static) -> Result<T> {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        rt.block_on(async move {
            let bus = tokio::time::timeout(Duration::from_secs(5), Connection::session())
                .await
                .map_err(|_| anyhow!("session bus: timed out"))??;
            tokio::time::timeout(Duration::from_secs(10), f(bus)).await.map_err(|_| anyhow!("GNOME did not answer"))?
        })
    })
    .join()
    .map_err(|_| anyhow!("display configuration thread"))?
}

async fn proxy(bus: &Connection) -> Result<Proxy<'static>> {
    let path: OwnedObjectPath = "/org/gnome/Mutter/DisplayConfig".try_into()?;
    Ok(Proxy::new(bus, DEST, path, DEST).await?)
}

async fn state(config: &Proxy<'static>) -> Result<(u32, Vec<Monitor>)> {
    let (serial, monitors, logical, _): State =
        config.call("GetCurrentState", &()).await.context("GNOME's monitor configuration")?;
    let mut list = Vec::new();
    for l in logical {
        for spec in &l.5 {
            let Some((_, modes, _)) = monitors.iter().find(|m| m.0.0 == spec.0) else { continue };
            let current = modes
                .iter()
                .find(|m| m.6.get("is-current").and_then(|v| bool::try_from(v).ok()).unwrap_or(false))
                .map(|m| m.0.clone());
            let Some(current) = current else { continue };
            list.push(Monitor {
                connector: spec.0.clone(),
                modes: modes
                    .iter()
                    .map(|m| Mode {
                        id: m.0.clone(),
                        width: m.1.max(0) as u32,
                        height: m.2.max(0) as u32,
                        refresh: m.3,
                        scales: m.5.clone(),
                    })
                    .collect(),
                current,
                x: l.0,
                y: l.1,
                scale: l.2,
                transform: l.3,
                primary: l.4,
            });
        }
    }
    Ok((serial, list))
}

/// GNOME lets programs change the monitors (not in every session: a remote
/// login of gnome-remote-desktop, for one), and says so.
pub fn available() -> bool {
    blocking(async |bus| {
        let config = proxy(&bus).await?;
        let allowed: bool = config.get_property("ApplyMonitorsConfigAllowed").await.unwrap_or(true);
        let (_, monitors) = state(&config).await?;
        Ok(allowed && !monitors.is_empty())
    })
    .unwrap_or(false)
}

/// The monitors turned on, the primary one first, then left to right.
pub fn monitors() -> Result<Vec<Monitor>> {
    let mut list = blocking(async |bus| Ok(state(&proxy(&bus).await?).await?.1))?;
    list.sort_by_key(|m| (!m.primary, m.x, m.y));
    Ok(list)
}

/// Puts `connector` in mode `mode_id`, the others as they are. Several monitors
/// side by side are laid out again left to right (GNOME refuses overlaps and
/// gaps); the scale stays when the new mode takes it, 1 otherwise.
pub fn set_mode(connector: &str, mode_id: &str) -> Result<()> {
    let (connector, mode_id) = (connector.to_string(), mode_id.to_string());
    blocking(async move |bus| {
        let config = proxy(&bus).await?;
        let (serial, mut monitors) = state(&config).await?;
        let target = monitors.iter_mut().find(|m| m.connector == connector).context("the screen is gone")?;
        let mode = target.mode(&mode_id).cloned().with_context(|| format!("no mode {mode_id} on {connector}"))?;
        if !mode.scales.iter().any(|s| (s - target.scale).abs() < 1e-3) {
            target.scale = 1.0;
        }
        target.current = mode_id.clone();
        // Left to right again with the new sizes, on the row of the top-left one.
        monitors.sort_by_key(|m| (m.x, m.y));
        if monitors.len() > 1 {
            let top = monitors.iter().map(|m| m.y).min().unwrap_or(0);
            let mut x = monitors[0].x;
            for m in &mut monitors {
                let (w, _) = m.logical(m.mode(&m.current).context("current mode")?);
                m.x = x;
                m.y = top;
                x += w;
            }
        }
        let logical: Vec<(i32, i32, f64, u32, bool, Vec<(String, String, HashMap<String, Value>)>)> = monitors
            .iter()
            .map(|m| (m.x, m.y, m.scale, m.transform, m.primary, vec![(m.connector.clone(), m.current.clone(), HashMap::new())]))
            .collect();
        let properties: HashMap<String, Value> = HashMap::new();
        config
            .call::<_, _, ()>("ApplyMonitorsConfig", &(serial, TEMPORARY, logical, properties))
            .await
            .context("GNOME refused the monitor configuration")?;
        Ok(())
    })
}

/// Mode of `connector` whose size is `width`x`height`, the refresh closest to
/// the current one.
pub fn mode_for(monitor: &Monitor, width: u32, height: u32) -> Result<String> {
    let current = monitor.mode(&monitor.current).map_or(60.0, |m| m.refresh);
    monitor
        .modes
        .iter()
        .filter(|m| (m.width, m.height) == (width, height))
        .min_by(|a, b| (a.refresh - current).abs().total_cmp(&(b.refresh - current).abs()))
        .map(|m| m.id.clone())
        .ok_or_else(|| anyhow!("no mode {width}x{height} on {}", monitor.connector))
}
