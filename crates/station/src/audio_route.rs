//! Put the kid's apps on a `tutor_apps` sink that loops back to their real
//! output, so Dad can hear the apps (its monitor) without hearing his own
//! voice, which plays on that output directly. The output follows the headset
//! as it's plugged in and out, and Dad can pick another. Undone on exit.

use std::process::Command;

use anyhow::{Context, Result, bail};

pub const APP_SINK: &str = "tutor_apps";
/// Our own streams (Dad's voice) must never be moved onto the app sink: that would echo.
const OWN_BINARY: &str = "tutor-station";

pub struct Route {
    /// Substring naming their preferred headset; empty means their defaults.
    pattern: String,
    /// Their real output, where Dad's voice and the loopback play.
    pub sink: String,
    /// Their mic.
    pub source: String,
    /// Their output's volume as last sent to Dad, to spot changes they make.
    pub volume: u8,
    loopback: Option<u32>,
}

impl Route {
    pub fn setup(pattern: &str) -> Result<Self> {
        let default_sink = pactl(&["get-default-sink"])?;
        let default_source = pactl(&["get-default-source"])?;
        // Either can be missing (headset unplugged, monitor asleep): the
        // device watcher fills it in when one appears.
        let sink = pick(&sink_names()?, pattern, &default_sink);
        let source = pick(&source_names()?, pattern, &default_source).unwrap_or_default();

        // Recreate rather than reuse: a leftover loopback may point at a device that has since gone.
        unload_app_sink();
        pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={APP_SINK}"),
            "sink_properties=device.description=Tutor-Apps",
        ])?;
        // The device-restore database had this sink at 150%, which clipped
        // everything Dad heard. Unity gain: their own volume is the headset's.
        pactl(&["set-sink-volume", APP_SINK, "100%"])?;
        let mut route = Self { pattern: pattern.to_string(), sink: String::new(), source, volume: 0, loopback: None };
        if let Some(sink) = sink
            && let Err(e) = route.set_sink(&sink)
        {
            tracing::warn!(error = format!("{e:#}"), "audio.output_failed");
        }
        pactl(&["set-default-sink", APP_SINK])?;
        for input in sink_inputs()? {
            // Never our loopback (a feedback loop) or our own streams; a Steam
            // call would echo Dad's voice back to him from the app sink.
            let skip = input.binary == OWN_BINARY
                || input.binary == "steamwebhelper"
                || input.module.is_some() && input.module == route.loopback;
            if !skip {
                let _ = pactl(&["move-sink-input", &input.id, APP_SINK]);
            }
        }
        tracing::info!(apps = APP_SINK, sink = %route.sink, source = %route.source, "audio.routed");
        Ok(route)
    }

    /// Point the app loopback at `sink`.
    pub fn set_sink(&mut self, sink: &str) -> Result<()> {
        if sink == self.sink && self.loopback.is_some() {
            return Ok(());
        }
        if let Some(id) = self.loopback.take() {
            let _ = pactl(&["unload-module", &id.to_string()]);
        }
        // A name of our own: module-stream-restore remembered the default
        // loopback name on the app sink and kept restoring it there, a
        // feedback loop that clipped everything.
        let id = pactl(&[
            "load-module",
            "module-loopback",
            &format!("source={APP_SINK}.monitor"),
            &format!("sink={sink}"),
            "latency_msec=20",
            "sink_dont_move=true",
            "sink_input_properties=media.name=tutor-apps-to-headset",
        ])?;
        self.loopback = id.trim().parse().ok();
        self.sink = sink.to_string();
        // Restore can still land a beat after the stream appears: check until it sticks.
        for _ in 0..20 {
            let mine: Vec<SinkInput> =
                sink_inputs()?.into_iter().filter(|i| i.module.is_some() && i.module == self.loopback).collect();
            if !mine.is_empty() && mine.iter().all(|i| i.sink == sink_id(sink)) {
                return Ok(());
            }
            for input in &mine {
                let _ = pactl(&["move-sink-input", &input.id, sink]);
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        bail!("loopback won't stay on {sink}")
    }

    /// After devices come or go: the output and mic we should be on now, where
    /// they differ from the current ones. The preferred headset wins whenever
    /// it's present; otherwise only a vanished device is replaced.
    pub fn wanted(&self) -> Result<(Option<String>, Option<String>)> {
        let choose = |names: Vec<String>, current: &str| -> Option<String> {
            let preferred =
                if self.pattern.is_empty() { None } else { names.iter().find(|n| usable(n) && n.contains(&self.pattern)).cloned() };
            match preferred {
                Some(p) if p != current => Some(p),
                Some(_) => None,
                None if names.iter().any(|n| n == current) => None,
                None => names.into_iter().find(|n| usable(n)),
            }
        };
        Ok((choose(sink_names()?, &self.sink), choose(source_names()?, &self.source)))
    }

    pub fn teardown(&self) {
        let _ = pactl(&["set-default-sink", &self.sink]);
        if let Ok(inputs) = sink_inputs() {
            for input in inputs {
                let _ = pactl(&["move-sink-input", &input.id, &self.sink]);
            }
        }
        unload_app_sink();
        tracing::info!(sink = %self.sink, "audio.restored");
    }
}

/// `sink`'s volume in percent (its first channel); `None` without one.
pub fn volume(sink: &str) -> Option<u8> {
    if sink.is_empty() {
        return None;
    }
    parse_volume(&pactl(&["get-sink-volume", sink]).ok()?)
}

pub fn set_volume(sink: &str, percent: u8) -> Result<()> {
    pactl(&["set-sink-volume", sink, &format!("{}%", percent.min(100))]).map(drop)
}

/// "Volume: front-left: 42597 /  65% / -11.23 dB,   front-right: …" → 65.
fn parse_volume(text: &str) -> Option<u8> {
    let percent = text.split('%').next()?.rsplit('/').next()?.trim().parse::<u32>().ok()?;
    Some(percent.min(255) as u8)
}

/// Whether `name` is one of their outputs (`kind` = "sinks") or mics ("sources") right now.
pub fn is_device(kind: &str, name: &str) -> bool {
    usable(name) && names(kind).is_ok_and(|names| names.iter().any(|n| n == name))
}

/// Their outputs (`kind` = "sinks") or mics ("sources") as (name, description);
/// the app sink and monitors excluded.
pub fn devices(kind: &str) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in pactl(&["list", kind])?.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Name: ") {
            out.push((name.to_string(), name.to_string()));
        } else if let Some(desc) = line.strip_prefix("Description: ")
            && let Some(last) = out.last_mut()
        {
            last.1 = desc.to_string();
        }
    }
    out.retain(|(n, _)| usable(n));
    Ok(out)
}

fn unload_app_sink() {
    let Ok(modules) = pactl(&["list", "short", "modules"]) else { return };
    let mut ids: Vec<u32> = modules
        .lines()
        .filter(|l| l.contains(APP_SINK))
        .filter_map(|l| l.split('\t').next()?.parse().ok())
        .collect();
    // Loopback first (higher id), then the sink it reads from.
    ids.sort_unstable_by(|a, b| b.cmp(a));
    for id in ids {
        let _ = pactl(&["unload-module", &id.to_string()]);
    }
}

/// A sink's index from its name.
fn sink_id(name: &str) -> Option<String> {
    let list = pactl(&["list", "short", "sinks"]).ok()?;
    list.lines().find(|l| l.split('\t').nth(1) == Some(name)).and_then(|l| l.split('\t').next()).map(str::to_string)
}

/// PipeWire's placeholder sink when it has no real output; nothing can be moved onto it.
const NO_OUTPUT: &str = "auto_null";

fn usable(name: &str) -> bool {
    name != APP_SINK && name != NO_OUTPUT && !name.starts_with(&format!("{APP_SINK}.")) && !name.ends_with(".monitor")
}

fn pick(names: &[String], pattern: &str, default: &str) -> Option<String> {
    if !pattern.is_empty()
        && let Some(n) = names.iter().find(|n| usable(n) && n.contains(pattern))
    {
        return Some(n.clone());
    }
    if usable(default) && names.iter().any(|n| n == default) {
        return Some(default.to_string());
    }
    names.iter().find(|n| usable(n)).cloned()
}

fn sink_names() -> Result<Vec<String>> {
    names("sinks")
}

fn source_names() -> Result<Vec<String>> {
    Ok(names("sources")?.into_iter().filter(|n| usable(n)).collect())
}

fn names(kind: &str) -> Result<Vec<String>> {
    Ok(pactl(&["list", "short", kind])?
        .lines()
        .filter_map(|l| l.split('\t').nth(1).map(str::to_string))
        .collect())
}

struct SinkInput {
    id: String,
    /// Index of the sink it plays on.
    sink: Option<String>,
    binary: String,
    /// The module that owns it (loopbacks), if any.
    module: Option<u32>,
}

fn sink_inputs() -> Result<Vec<SinkInput>> {
    let mut out: Vec<SinkInput> = Vec::new();
    for line in pactl(&["list", "sink-inputs"])?.lines() {
        let trimmed = line.trim();
        if let Some(rest) = line.strip_prefix("Sink Input #") {
            out.push(SinkInput { id: rest.trim().to_string(), sink: None, binary: String::new(), module: None });
        } else if let Some(last) = out.last_mut() {
            if let Some(v) = trimmed.strip_prefix("application.process.binary = ") {
                last.binary = v.trim_matches('"').to_string();
            } else if let Some(v) = trimmed.strip_prefix("Sink: ") {
                last.sink = Some(v.to_string());
            } else if let Some(v) = trimmed.strip_prefix("Owner Module: ") {
                last.module = v.parse().ok();
            }
        }
    }
    Ok(out)
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl").args(args).output().context("run pactl")?;
    if !out.status.success() {
        bail!("pactl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_past_the_app_sink() {
        let sinks = vec!["tutor_apps".to_string(), "alsa_output.hdmi".to_string()];
        assert_eq!(pick(&sinks, "Jabra", "tutor_apps").as_deref(), Some("alsa_output.hdmi"));
        let sinks = vec!["alsa_output.Jabra".to_string(), "tutor_apps".to_string()];
        assert_eq!(pick(&sinks, "Jabra", "tutor_apps").as_deref(), Some("alsa_output.Jabra"));
    }

    #[test]
    fn reads_the_volume() {
        let text = "Volume: front-left: 42597 /  65% / -11.23 dB,   front-right: 42597 /  65% / -11.23 dB\n        balance 0.00";
        assert_eq!(parse_volume(text), Some(65));
        assert_eq!(parse_volume("Volume: mono: 65536 / 100% / 0.00 dB"), Some(100));
    }

    #[test]
    fn no_output_is_not_a_device() {
        let sinks = vec!["tutor_apps".to_string(), "auto_null".to_string()];
        assert_eq!(pick(&sinks, "Jabra", "auto_null"), None);
    }
}
