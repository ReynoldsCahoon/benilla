//! `WOW_PROBE_CHAT`: chat and GM lines typed into a live session, on its own so a player build
//! can carry it with the `chat-probe` feature (the iPad's memory tests teleport the probe
//! through zones and maps) as `probe_route` and `probe_crowd` are carried.

use bevy::prelude::*;

/// The wall clock: probe schedules must not follow the virtual one (`capture::probes`).
type ProbeClock<'w> = Res<'w, Time<bevy::time::Real>>;

/// `WOW_PROBE_CHAT="<line>[;<line>…]"`: types each line into the chat box once in-world, from
/// `WOW_PROBE_CHAT_AT` seconds (default 8), `WOW_PROBE_CHAT_EVERY` seconds apart (default one
/// burst). Spacing matters: two field flips inside one drain merge to a no-op.
pub(crate) struct ProbeChatPlugin;

impl Plugin for ProbeChatPlugin {
    fn build(&self, app: &mut App) {
        let lines = std::env::var("WOW_PROBE_CHAT").unwrap_or_default();
        let at = std::env::var("WOW_PROBE_CHAT_AT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8.0);
        let every = std::env::var("WOW_PROBE_CHAT_EVERY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0);
        app.insert_resource(ProbeChat {
            lines,
            at,
            every,
            sent: 0,
        })
        .add_systems(Update, fire_probe_chat);
    }
}

/// [`ProbeChatPlugin`] state; `every` of 0 sends every line in one burst.
#[derive(Resource)]
struct ProbeChat {
    lines: String,
    at: f32,
    every: f32,
    sent: usize,
}

/// Submits due lines once a self player exists (the server drops a `.go` sent before world entry),
/// through the chat box so client-side slash commands parse as typed ones do.
fn fire_probe_chat(
    mut probe: ResMut<ProbeChat>,
    time: ProbeClock,
    mut hosted_vm: crate::script_calls::HostedVm,
    self_player: Query<(), With<crate::net::SelfPlayer>>,
) {
    hosted_vm.run(|script| {
        if probe.lines.is_empty() {
            return;
        }
        if self_player.is_empty() {
            return; // not in-world yet
        }
        let Some(script) = script else {
            return;
        };
        // Line N is due at `at + N·every`.
        loop {
            let Some(line) = probe
                .lines
                .split(';')
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .nth(probe.sent)
            else {
                return; // all sent
            };
            let due = probe.at + probe.every * probe.sent as f32;
            if time.elapsed_secs() < due {
                return;
            }
            info!("probe-chat: sending {line:?}");
            script.push_chat_input(line.to_string());
            probe.sent += 1;
        }
    })
}
