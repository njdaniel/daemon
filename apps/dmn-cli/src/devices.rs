//! `dmn devices`: the live PipeWire node table as two lists (docs/SPEC.md § Discovery).

use std::fmt::Write as _;
use std::time::Duration;

use dmn_audio::{NodeTable, PipeWireContext, PipeWireError};
use dmn_core::Config;

/// How long to wait for the first registry enumeration before printing what we have.
const SYNC_TIMEOUT: Duration = Duration::from_millis(500);

pub fn run(cfg: &Config) -> Result<String, String> {
    let ctx = PipeWireContext::connect().map_err(connect_error)?;
    let table = ctx.wait_for_sync(SYNC_TIMEOUT);
    if let Some(why) = &table.disconnected {
        return Err(connect_error(PipeWireError::Connect(why.clone())));
    }
    Ok(render(&table, &cfg.audio.mic, &cfg.audio.app_stream))
}

fn connect_error(e: PipeWireError) -> String {
    format!("{e}\n     fix: systemctl --user start pipewire pipewire-pulse wireplumber")
}

/// Two sections. `*` marks PipeWire's default source and `>` the node config selects
/// (`[audio].mic` by `node.name`, `[audio].app_stream` by `application.name`).
pub fn render(table: &NodeTable, mic_cfg: &str, app_cfg: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Capture sources (media.class = Audio/Source)");
    let mut any = false;
    for n in table.sources() {
        any = true;
        let is_default = table.default_source.as_deref() == Some(n.name.as_str());
        let selected = mic_cfg == n.name || (mic_cfg == "default" && is_default);
        let _ = writeln!(
            s,
            "  {}{} {:<44} {}",
            if selected { '>' } else { ' ' },
            if is_default { '*' } else { ' ' },
            n.name,
            n.description
        );
    }
    if !any {
        let _ = writeln!(s, "   (none)");
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "App streams (media.class = Stream/Output/Audio)");
    any = false;
    for st in table.streams() {
        any = true;
        let selected = !app_cfg.is_empty() && st.application == app_cfg;
        let _ = writeln!(
            s,
            "  {}  {} — {}   [{}]",
            if selected { '>' } else { ' ' },
            st.application,
            st.media_name,
            st.node_name
        );
    }
    if !any {
        let _ = writeln!(s, "     (none playing)");
    }
    if !table.synced {
        let _ = writeln!(
            s,
            "\n(registry not fully enumerated within {SYNC_TIMEOUT:?})"
        );
    }
    let _ = writeln!(
        s,
        "\n* = PipeWire default source   > = selected by config ([audio].mic = {mic_cfg:?}, [audio].app_stream = {app_cfg:?})"
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use dmn_audio::RegistryEvent;

    fn table() -> NodeTable {
        let mut t = NodeTable::default();
        let add = |t: &mut NodeTable, id, props: &[(&str, &str)]| {
            t.apply(RegistryEvent::Added {
                id,
                props: props
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            });
        };
        add(
            &mut t,
            1,
            &[
                ("media.class", "Audio/Source"),
                ("node.name", "alsa_input.usb"),
                ("node.description", "USB Mic"),
            ],
        );
        add(
            &mut t,
            2,
            &[
                ("media.class", "Stream/Output/Audio"),
                ("application.name", "Firefox"),
                ("media.name", "Google Meet"),
                ("node.name", "firefox"),
            ],
        );
        t.apply(RegistryEvent::DefaultSource(Some("alsa_input.usb".into())));
        t.apply(RegistryEvent::Synced);
        t
    }

    #[test]
    fn renders_both_sections_with_markers() {
        let out = render(&table(), "default", "Firefox");
        assert!(out.contains(">* alsa_input.usb"), "{out}");
        assert!(out.contains("USB Mic"), "{out}");
        assert!(
            out.contains(">  Firefox — Google Meet   [firefox]"),
            "{out}"
        );
        assert!(!out.contains("not fully enumerated"), "{out}");
    }

    #[test]
    fn empty_table_says_so_and_flags_missing_sync() {
        let out = render(&NodeTable::default(), "default", "");
        assert!(out.contains("(none)"), "{out}");
        assert!(out.contains("(none playing)"), "{out}");
        assert!(out.contains("not fully enumerated"), "{out}");
    }
}
