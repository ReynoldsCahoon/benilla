//! `WOW_PROBE_QUEST=<plan file>`: a bot that plays the character through a starting zone's quests
//! with no one at the keys, so a memory soak repeats a player's session: talking to quest givers,
//! fighting, looting and turning in, over and over.
//!
//! The plan (the wow-optimize workbench's `tools/questplan.py`, from the server's world DB) says
//! what a player would look up: the zone's quests, who gives and takes each, what each objective
//! needs, which creatures or objects drop each item, and where things spawn. Everything else is
//! read live from the client, as a player reads the screen: the units in view and their state, the
//! quest givers' marks, the quest log's objectives, the open windows.
//!
//! Every action goes through a player's own paths:
//!
//! - Movement holds the movement bindings and turns the character, as the route walker does
//!   ([`super::probe_route`]). Stuck (holding forward, not moving): back up, side-step, retry;
//!   after [`UNSTICK_TRIES`] the target is set aside and another chosen. It never teleports.
//! - Interaction is a right-click at the unit's or object's place on screen, through the window's
//!   cursor and button, the same input a mouse or a tap ([`crate::touch_mouse`]) writes.
//! - Windows are worked through the stock interface: its buttons' `Click()` and the 1.12 calls
//!   their scripts make (loot slots, quest accept, complete and reward, gossip choices).
//! - Combat is the right-click's attack plus the action bar: each usable spell on it, in order.
//!
//! GM help, typed into chat as a player would: a deep health pool (`.modify hp`, the test's
//! invulnerability), set again when it runs low or the level changes. No spawning, no teleport.
//!
//! Switches: `WOW_PROBE_QUEST_AT` (seconds after world entry, default 10), `WOW_PROBE_QUEST_HP`
//! (the pool, default 5000), `WOW_PROBE_QUEST_SECS` (exit after, default never). Logs
//! `PROBE_QUEST:` lines: each decision that changes, quests taken and turned in, kills, loot,
//! stuck legs, and a summary every minute.

use std::collections::HashMap;

use benilla_protocol::EntityKind;
use benilla_ui::script::{BindingInput, HeldInput};
use bevy::input::mouse::MouseButtonInput;
use bevy::input::ButtonState;
use bevy::prelude::*;
use bevy::window::{CursorMoved, WindowEvent};

use crate::net::{Guid, NetEntity, ObjectStore, SelfPlayer};
use crate::player::Player;
use benilla_world::schedule::WorldStage;

/// The wall clock: probe schedules must not follow the virtual one.
type ProbeClock<'w> = Res<'w, Time<bevy::time::Real>>;

/// Decisions are made this often (seconds); movement runs every frame.
const THINK_SECS: f32 = 0.25;
/// Yards from a hostile target to stop and fight.
const MELEE_YD: f32 = 3.0;
/// Yards from an NPC, object or corpse to stop and right-click: inside the 5.5556 yd service reach.
const USE_YD: f32 = 3.5;
/// How far a unit counts as in view for choosing targets (the server's visibility is ~100 yd).
const SEEK_YD: f32 = 90.0;
/// Holding forward and moved less than this in [`STUCK_WINDOW`] seconds: stuck.
const STUCK_YD: f32 = 0.6;
const STUCK_WINDOW: f32 = 1.5;
/// Back-up and side-step legs, seconds each.
const UNSTICK_LEG: f32 = 0.8;
/// Unstick attempts on one target before it is set aside.
const UNSTICK_TRIES: u32 = 6;
/// How long a set-aside target stays aside, seconds.
const ASIDE_SECS: f32 = 120.0;
/// Right-clicks on one goal before it is set aside (a click that never takes).
const CLICK_TRIES: u32 = 6;
/// `DialogStatus`: an offer (`!`) and a reward (`?`), the two marks worth a walk.
const STATUS_AVAILABLE: u32 = 5;
const STATUS_REWARD: [u32; 2] = [6, 7];

// ---- The plan ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Creature,
    Object,
}

#[derive(Default, Debug)]
pub(crate) struct PlanQuest {
    pub(crate) title: String,
    pub(crate) min_level: u32,
    /// The quest that must be turned in first (`PrevQuestId` > 0); 0 for none.
    pub(crate) prev: u32,
    pub(crate) givers: Vec<(Kind, u32)>,
    pub(crate) enders: Vec<(Kind, u32)>,
    /// Kill or use objectives: `(kind, entry, count)`.
    pub(crate) kills: Vec<(Kind, u32, u32)>,
    /// Items to bring: `(item, count, name)`.
    pub(crate) items: Vec<(u32, u32, String)>,
}

#[derive(Default, Debug)]
pub(crate) struct Plan {
    pub(crate) quests: Vec<(u32, PlanQuest)>,
    /// Who drops each item.
    pub(crate) droppers: HashMap<u32, Vec<(Kind, u32)>>,
    /// Names and `UNIT_NPC_FLAGS` by `(kind, entry)`.
    pub(crate) names: HashMap<(Kind, u32), (String, u32)>,
    pub(crate) spawns: HashMap<(Kind, u32), Vec<[f32; 3]>>,
    /// The zone's walk graph, from the server's navmesh: node positions and each node's neighbours.
    pub(crate) nodes: Vec<[f32; 3]>,
    pub(crate) edges: Vec<Vec<usize>>,
}

fn kind(s: &str) -> Option<Kind> {
    match s {
        "c" => Some(Kind::Creature),
        "g" => Some(Kind::Object),
        _ => None,
    }
}

impl Plan {
    pub(crate) fn parse(text: &str) -> Plan {
        let mut plan = Plan::default();
        let mut index: HashMap<u32, usize> = HashMap::new();
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let n = |i: usize| f.get(i).and_then(|v| v.parse::<u32>().ok());
            let fl = |i: usize| f.get(i).and_then(|v| v.parse::<f32>().ok());
            match f.first().copied() {
                Some("Q") => {
                    let (Some(id), Some(title)) = (n(1), f.get(4)) else {
                        continue;
                    };
                    index.insert(id, plan.quests.len());
                    plan.quests.push((
                        id,
                        PlanQuest {
                            title: (*title).to_string(),
                            min_level: n(2).unwrap_or(1),
                            prev: f.get(3).and_then(|v| v.parse::<i64>().ok()).map_or(0, |p| {
                                if p > 0 {
                                    p as u32
                                } else {
                                    0
                                }
                            }),
                            ..default()
                        },
                    ));
                }
                Some(tag @ ("QG" | "QT" | "QK")) => {
                    let (Some(id), Some(k), Some(e)) = (n(1), f.get(2).and_then(|s| kind(s)), n(3))
                    else {
                        continue;
                    };
                    let Some(q) = index.get(&id).map(|&i| &mut plan.quests[i].1) else {
                        continue;
                    };
                    match tag {
                        "QG" => q.givers.push((k, e)),
                        "QT" => q.enders.push((k, e)),
                        _ => q.kills.push((k, e, n(4).unwrap_or(1))),
                    }
                }
                Some("QI") => {
                    let (Some(id), Some(item), Some(count)) = (n(1), n(2), n(3)) else {
                        continue;
                    };
                    let name = f.get(4).map_or(String::new(), |s| (*s).to_string());
                    if let Some(q) = index.get(&id).map(|&i| &mut plan.quests[i].1) {
                        q.items.push((item, count, name));
                    }
                }
                Some("D") => {
                    if let (Some(item), Some(k), Some(e)) =
                        (n(1), f.get(2).and_then(|s| kind(s)), n(3))
                    {
                        plan.droppers.entry(item).or_default().push((k, e));
                    }
                }
                Some("N") => {
                    if let (Some(k), Some(e), Some(flags), Some(name)) =
                        (f.get(1).and_then(|s| kind(s)), n(2), n(3), f.get(4))
                    {
                        plan.names.insert((k, e), ((*name).to_string(), flags));
                    }
                }
                Some("W") => {
                    if let (Some(i), Some(x), Some(y), Some(z)) = (n(1), fl(2), fl(3), fl(4)) {
                        let i = i as usize;
                        if plan.nodes.len() <= i {
                            plan.nodes.resize(i + 1, [0.0; 3]);
                            plan.edges.resize(i + 1, Vec::new());
                        }
                        plan.nodes[i] = [x, y, z];
                    }
                }
                Some("E") => {
                    if let (Some(a), Some(b)) = (n(1), n(2)) {
                        let (a, b) = (a as usize, b as usize);
                        if a < plan.edges.len() && b < plan.edges.len() {
                            plan.edges[a].push(b);
                            plan.edges[b].push(a);
                        }
                    }
                }
                Some("S") => {
                    if let (Some(k), Some(e), Some(x), Some(y), Some(z)) =
                        (f.get(1).and_then(|s| kind(s)), n(2), fl(3), fl(4), fl(5))
                    {
                        plan.spawns.entry((k, e)).or_default().push([x, y, z]);
                    }
                }
                _ => {}
            }
        }
        plan
    }

    /// The creature or object entries named `name` (a quest objective's leaderboard text).
    fn entries_named(&self, name: &str) -> Vec<(Kind, u32)> {
        self.names
            .iter()
            .filter(|(_, (n, _))| n.eq_ignore_ascii_case(name))
            .map(|(&k, _)| k)
            .collect()
    }

    /// The waypoints from `from` to `to` through the walk graph: the node nearest each end and the
    /// shortest way between them (Dijkstra on the plane). `None` without a graph.
    pub(crate) fn route(&self, from: [f32; 2], to: [f32; 2]) -> Option<Vec<[f32; 2]>> {
        let near = |p: [f32; 2]| {
            (0..self.nodes.len()).min_by(|&a, &b| {
                let (na, nb) = (self.nodes[a], self.nodes[b]);
                dist2(p, [na[0], na[1]]).total_cmp(&dist2(p, [nb[0], nb[1]]))
            })
        };
        let (s, e) = (near(from)?, near(to)?);
        let n = self.nodes.len();
        let mut best = vec![f32::INFINITY; n];
        let mut prev = vec![usize::MAX; n];
        let mut done = vec![false; n];
        best[s] = 0.0;
        while let Some(u) = (0..n)
            .filter(|&i| !done[i] && best[i].is_finite())
            .min_by(|&a, &b| best[a].total_cmp(&best[b]))
        {
            if u == e {
                break;
            }
            done[u] = true;
            for &v in &self.edges[u] {
                let (pu, pv) = (self.nodes[u], self.nodes[v]);
                let d = best[u] + dist2([pu[0], pu[1]], [pv[0], pv[1]]);
                if d < best[v] {
                    best[v] = d;
                    prev[v] = u;
                }
            }
        }
        if !best[e].is_finite() {
            return None;
        }
        let mut path = vec![];
        let mut at = e;
        while at != usize::MAX {
            path.push([self.nodes[at][0], self.nodes[at][1]]);
            at = prev[at];
        }
        path.reverse();
        path.push(to);
        Some(path)
    }

    fn quest_titled(&self, title: &str) -> Option<&PlanQuest> {
        self.quests
            .iter()
            .map(|(_, q)| q)
            .find(|q| q.title == title)
    }
}

// ---- The quest log, as the Lua side reports it ----

/// One quest log entry: its title, whether it is complete, and its objectives `(kind, done, name)`
/// where kind is the leaderboard type (`monster`, `item`, `object`, `event`).
#[derive(Debug, PartialEq)]
pub(crate) struct LogQuest {
    pub(crate) title: String,
    pub(crate) complete: bool,
    pub(crate) objectives: Vec<(String, bool, String)>,
}

/// The objective's subject in a leaderboard line: "Mottled Boar slain: 3/10" is "Mottled Boar",
/// "Scorpid Worker Tail: 2/10" is "Scorpid Worker Tail".
pub(crate) fn objective_name(text: &str) -> String {
    let head = text.rsplit_once(':').map_or(text, |(h, _)| h).trim();
    head.strip_suffix(" slain").unwrap_or(head).to_string()
}

pub(crate) fn parse_log(text: &str) -> Vec<LogQuest> {
    text.lines()
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut f = l.split('\t');
            let title = f.next()?.to_string();
            let complete = f.next()? == "1";
            let objectives = f
                .filter_map(|o| {
                    let mut p = o.splitn(3, '|');
                    let ty = p.next()?.to_string();
                    let done = p.next()? == "1";
                    Some((ty, done, objective_name(p.next()?)))
                })
                .collect();
            Some(LogQuest {
                title,
                complete,
                objectives,
            })
        })
        .collect()
}

/// The Lua half: reads the quest log and works whatever window is open, through the stock
/// interface's buttons where a player would click one.
const LUA: &str = r#"
BenillaBot = BenillaBot or {}
QUEST_FADING_DISABLE = "1"
-- Auto loot, as the user plays: a right-click on a corpse takes everything it can.
SetCVar("autoLootDefault", "1")
-- The camera returns behind the character always, standing at an NPC too, so what it faces is
-- in front of the camera for the click.
SetCVar("cameraSmoothStyle", "2")
function BenillaBot.Log()
  local out = ""
  for i = 1, GetNumQuestLogEntries() do
    local title, level, tag, isHeader, collapsed, isComplete = GetQuestLogTitle(i)
    if not isHeader and title then
      SelectQuestLogEntry(i)
      local n = GetNumQuestLeaderBoards()
      local done = (isComplete == 1 or (n == 0 and isComplete ~= -1)) and "1" or "0"
      local s = title .. "\t" .. done
      for j = 1, n do
        local text, typ, fin = GetQuestLogLeaderBoard(j)
        s = s .. "\t" .. (typ or "?") .. "|" .. (fin and "1" or "0") .. "|" .. (text or "")
      end
      out = out .. s .. "\n"
    end
  end
  return out
end
function BenillaBot.Complete(title)
  for i = 1, GetNumQuestLogEntries() do
    local t, level, tag, isHeader, collapsed, isComplete = GetQuestLogTitle(i)
    if t == title and not isHeader then
      SelectQuestLogEntry(i)
      return isComplete == 1 or (GetNumQuestLeaderBoards() == 0 and isComplete ~= -1)
    end
  end
  return false
end
function BenillaBot.Pick(buttons, prefix, n)
  -- A turn-in first (an active quest the log calls complete), else an offer.
  for i = 1, n do
    local b = getglobal(prefix .. i)
    if b and b:IsVisible() and b.isActive == 1 and BenillaBot.Complete(b:GetText()) then
      b:Click() return "turnin " .. b:GetText()
    end
    if b and b:IsVisible() and b.type == "Active" and BenillaBot.Complete(b:GetText()) then
      b:Click() return "turnin " .. b:GetText()
    end
  end
  for i = 1, n do
    local b = getglobal(prefix .. i)
    if b and b:IsVisible() and (b.isActive == 0 or b.type == "Available") then
      b:Click() return "offer " .. b:GetText()
    end
  end
  return nil
end
function BenillaBot.Dialog()
  if not (LootFrame and LootFrame:IsVisible()) then BenillaBot.lootTicks = 0 end
  if LootFrame and LootFrame:IsVisible() then
    -- A row that never goes (bags full, a unique item held) would hold the window open forever.
    BenillaBot.lootTicks = (BenillaBot.lootTicks or 0) + 1
    if BenillaBot.lootTicks > 12 then
      BenillaBot.lootTicks = 0
      CloseLoot() return "loot-stuck"
    end
    -- A mouse click on a row takes it: 1.12's LootSlot is only the bind-on-pickup confirmation,
    -- and a scripted Click() on a LootButton does nothing. Report the first row's place on screen
    -- (UIParent units, y up) for the bot's mouse; the window closes itself once empty, else page
    -- down, else close it.
    for i = 1, (LOOTFRAME_NUMBUTTONS or 4) do
      local b = getglobal("LootButton" .. i)
      if b and b:IsVisible() then
        local x, y = b:GetCenter()
        local k = b:GetEffectiveScale() / UIParent:GetEffectiveScale()
        return "lootclick " .. (x * k) .. " " .. (y * k) .. " " .. UIParent:GetWidth() .. " " .. UIParent:GetHeight()
      end
    end
    if LootFrameDownButton and LootFrameDownButton:IsVisible() then
      LootFrameDownButton:Click() return "loot-page"
    end
    CloseLoot() return "loot-done"
  end
  if QuestFrameDetailPanel and QuestFrameDetailPanel:IsVisible() then
    QuestFrameAcceptButton:Click() return "accept " .. (GetTitleText() or "")
  end
  if QuestFrameProgressPanel and QuestFrameProgressPanel:IsVisible() then
    if IsQuestCompletable() then QuestFrameCompleteButton:Click() return "complete" end
    QuestFrameGoodbyeButton:Click() return "progress-not-done"
  end
  if QuestFrameRewardPanel and QuestFrameRewardPanel:IsVisible() then
    if GetNumQuestChoices() > 0 then QuestFrameRewardPanel.itemChoice = 1 end
    QuestFrameCompleteQuestButton:Click() return "reward " .. (GetTitleText() or "")
  end
  if QuestFrameGreetingPanel and QuestFrameGreetingPanel:IsVisible() then
    local r = BenillaBot.Pick(nil, "QuestTitleButton", 32)
    if r then return r end
    QuestFrameGreetingGoodbyeButton:Click() return "greeting-nothing"
  end
  if GossipFrame and GossipFrame:IsVisible() then
    local r = BenillaBot.Pick(nil, "GossipTitleButton", 32)
    if r then return r end
    CloseGossip() return "gossip-nothing"
  end
  if MerchantFrame and MerchantFrame:IsVisible() then
    -- A merchant is only opened to sell: everything but quest items and the hearthstone.
    local n = BenillaBot.Sell()
    CloseMerchant() return "sold " .. n
  end
  if ClassTrainerFrame and ClassTrainerFrame:IsVisible() then CloseTrainer() return "trainer-close" end
  return ""
end
-- The action button shown in place i: a stance's bar (BonusActionButton, slots 73-84 for a
-- warrior in Battle Stance) covers the main bar while it is up, as 1.12 shows it.
function BenillaBot.Button(i)
  local prefix = "ActionButton"
  if BonusActionBarFrame and BonusActionBarFrame:IsVisible() then prefix = "BonusActionButton" end
  local b = getglobal(prefix .. i)
  return b, b and ActionButton_GetPagedID(b)
end
function BenillaBot.Fight()
  if not UnitExists("target") or UnitIsDead("target") or not UnitCanAttack("player", "target") then
    return ""
  end
  -- The buttons on screen, each on the slot it shows.
  for i = 1, 12 do
    local b, s = BenillaBot.Button(i)
    if s and HasAction(s) and not IsAttackAction(s) and not IsCurrentAction(s) and (GetActionCount(s) or 0) == 0 then
      local usable, nomana = IsUsableAction(s)
      local start, dur = GetActionCooldown(s)
      if usable and (dur or 0) == 0 and IsActionInRange(s) ~= 0 then
        b:Click() return "button " .. i .. " (slot " .. s .. ")"
      end
    end
  end
  return ""
end
function BenillaBot.Bar()
  local out = ""
  for i = 1, 12 do
    local b, s = BenillaBot.Button(i)
    if s and HasAction(s) then
      out = out .. i .. ":" .. s .. "=" .. (GetActionTexture(s) or "?") .. (IsAttackAction(s) and "(attack)" or "") .. " "
    end
  end
  return out
end
function BenillaBot.Attacking()
  for i = 1, 12 do
    local b, s = BenillaBot.Button(i)
    if s and HasAction(s) and IsAttackAction(s) then return IsCurrentAction(s) and true or false end
  end
  return false
end
-- Bags: free slots, what to keep, selling at an open merchant, destroying the least valuable.
function BenillaBot.Free()
  local free = 0
  for b = 0, 4 do
    for s = 1, (GetContainerNumSlots(b) or 0) do
      if not GetContainerItemLink(b, s) then free = free + 1 end
    end
  end
  return free
end
function BenillaBot.Keep(b, s)
  local link = GetContainerItemLink(b, s)
  if not link then return true end
  local _, _, id = string.find(link, "item:(%d+)")
  id = tonumber(id)
  if id == 6948 then return true end
  local name, l, quality, minLevel, typ = GetItemInfo(id)
  return typ == "Quest"
end
function BenillaBot.Sell()
  local n = 0
  for b = 0, 4 do
    for s = 1, (GetContainerNumSlots(b) or 0) do
      if not BenillaBot.Keep(b, s) then UseContainerItem(b, s) n = n + 1 end
    end
  end
  return n
end
function BenillaBot.Destroy()
  local bb, bs, bq
  for b = 0, 4 do
    for s = 1, (GetContainerNumSlots(b) or 0) do
      if not BenillaBot.Keep(b, s) then
        local tex, count, locked, quality = GetContainerItemInfo(b, s)
        quality = quality or 0
        if not bq or quality < bq then bb, bs, bq = b, s, quality end
      end
    end
  end
  if not bb then return "" end
  local link = GetContainerItemLink(bb, bs) or "?"
  PickupContainerItem(bb, bs)
  DeleteCursorItem()
  return "destroyed " .. link
end
function BenillaBot.State()
  local combat = UnitAffectingCombat("player") and "1" or "0"
  local hp = UnitHealth("player") or 0
  local maxhp = UnitHealthMax("player") or 1
  local attacking = BenillaBot.Attacking() and "1" or "0"
  return combat .. "\t" .. hp .. "\t" .. maxhp .. "\t" .. (UnitLevel("player") or 0) .. "\t" .. attacking .. "\t" .. BenillaBot.Free()
end
"#;

// ---- The bot ----

/// What the bot is doing; logged when it changes.
#[derive(Clone, Debug, PartialEq)]
enum Goal {
    Idle,
    /// Walk to a point, then decide again (a spawn of what is wanted, not in view).
    Walk {
        to: [f32; 2],
        why: String,
    },
    /// Walk to a unit or object and right-click it: talk, loot, use.
    Use {
        entity: Entity,
        guid: u64,
        why: String,
    },
    /// Close in on a hostile unit, right-click it to attack, and fight.
    Fight {
        entity: Entity,
        guid: u64,
        why: String,
    },
}

impl Goal {
    fn guid(&self) -> Option<u64> {
        match self {
            Goal::Use { guid, .. } | Goal::Fight { guid, .. } => Some(*guid),
            _ => None,
        }
    }
    fn label(&self) -> String {
        match self {
            Goal::Idle => "idle".into(),
            Goal::Walk { to, why } => format!("walk ({:.0}, {:.0}) for {why}", to[0], to[1]),
            Goal::Use { guid, why, .. } => format!("use {guid:#x} ({why})"),
            Goal::Fight { guid, why, .. } => format!("fight {guid:#x} ({why})"),
        }
    }
}

/// The unstick sequence: back up, side-step, then walk off at an angle from the goal's bearing
/// (try `n` turns [`DETOUR_DEG`]`[n]`) for longer each try, before heading for the goal again.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Unstick {
    None,
    Back { until: f32 },
    Side { until: f32, left: bool },
    Detour { until: f32, yaw: f32 },
}

/// The detour's turn from the goal's bearing, by try: alternate sides, wider each pair.
const DETOUR_DEG: [f32; 6] = [60.0, -60.0, 100.0, -100.0, 140.0, -140.0];

/// The detour leg for try `n` (1-based): longer each try.
fn detour(n: u32, bearing: f32) -> (f32, f32) {
    let i = (n.max(1) as usize - 1).min(DETOUR_DEG.len() - 1);
    (bearing + DETOUR_DEG[i].to_radians(), 1.5 * n as f32)
}

#[derive(Resource)]
struct QuestBot {
    plan: Plan,
    at: f32,
    hp_pool: u32,
    quit_after: Option<f32>,
    entered: Option<f32>,
    next_think: f32,
    goal: Goal,
    /// The goal's start, its clicks so far, its unstick tries.
    goal_since: f32,
    clicks: u32,
    last_click: f32,
    tries: u32,
    unstick: Unstick,
    /// The waypoints left on a walk-graph route to the goal, taken after a stuck.
    route: Option<Vec<[f32; 2]>>,
    /// Where the bot has walked, a point every 2 yd, newest last: the way back out of a place.
    trail: Vec<[f32; 2]>,
    /// This goal's way past walls: 0 nothing tried, 1 the graph checked ahead, then on stucks
    /// 2 retraced the trail, 3 took the graph's route; past that the detours.
    escape: u8,
    /// Position samples while walking, for the stuck test.
    moved_from: Option<([f32; 2], f32)>,
    /// Targets set aside, by guid, until a time.
    aside: HashMap<u64, f32>,
    /// Spawn points walked to and found empty, until a time.
    visited: Vec<([f32; 2], f32)>,
    /// Corpses already looted.
    looted: Vec<u64>,
    /// Stucks on each target across goals, so switching between two unreachable targets cannot
    /// reset the count; a target at [`UNSTICK_TRIES`] is set aside.
    stuck_on: HashMap<u64, u32>,
    /// The fight target the last "nearer" switch left, never switched straight back to.
    left_target: Option<u64>,
    /// Quests turned in this run, by title.
    done: Vec<String>,
    walking: bool,
    level: u32,
    // Counters for the summary.
    kills: u32,
    loots: u32,
    accepted: u32,
    turned_in: u32,
    stuck: u32,
    last_summary: f32,
}

/// The bot's exchange with the Lua side, which runs in a system of its own (the VM's host may share
/// a system with nothing that touches the world): requests one way, readings the other.
#[derive(Resource)]
struct BotLua {
    hp_pool: u32,
    set_up: bool,
    /// Take readings (and work any open window) on the next pass.
    pending: bool,
    /// Use the action bar once on the next pass.
    fight: bool,
    /// Destroy the least valuable bag item on the next pass (bags full, no vendor to sell to).
    destroy: bool,
    /// Chat lines to type.
    chat: Vec<String>,
    /// The open window's outcome, the player's state and the quest log, as the Lua half reports.
    readings: Option<(String, String, String)>,
}

/// The Lua half, after the bot's think: sets itself up once, then types the asked chat lines,
/// uses the action bar when asked, and takes the readings.
fn run_bot_lua(mut lua: ResMut<BotLua>, mut hosted_vm: crate::script_calls::HostedVm) {
    if !lua.pending && !lua.fight && !lua.destroy && lua.chat.is_empty() {
        return;
    }
    hosted_vm.run(|script| {
        let Some(script) = script else {
            return;
        };
        if !lua.set_up {
            if let Err(e) = script.run(LUA) {
                warn!("PROBE_QUEST: the Lua half failed to load: {e}");
                return;
            }
            let hp = lua.hp_pool;
            script.push_chat_input(format!(".modify hp {hp}"));
            lua.set_up = true;
            info!("PROBE_QUEST: set up (hp pool {hp}, instant quest text, auto loot)");
            let bar: String = script
                .eval("return BenillaBot.Bar()")
                .unwrap_or_else(|e| format!("error: {e}"));
            let diag: String = script
                .eval(
                    "return 'offset=' .. tostring(GetBonusBarOffset()) .. ' page=' .. \
                     tostring(CURRENT_ACTIONBAR_PAGE) .. ' paged2=' .. \
                     tostring(ActionButton_GetPagedID(ActionButton2)) .. ' has74=' .. \
                     tostring(HasAction(74)) .. ' has2=' .. tostring(HasAction(2))",
                )
                .unwrap_or_else(|e| format!("error: {e}"));
            info!("PROBE_QUEST: action bar {bar} ({diag})");
        }
        for line in std::mem::take(&mut lua.chat) {
            script.push_chat_input(line);
        }
        if std::mem::take(&mut lua.fight) {
            let used: String = script
                .eval("return BenillaBot.Fight()")
                .unwrap_or_else(|e| format!("error: {e}"));
            if !used.is_empty() {
                info!("PROBE_QUEST: action bar: {used}");
            }
        }
        if std::mem::take(&mut lua.destroy) {
            let gone: String = script
                .eval("return BenillaBot.Destroy()")
                .unwrap_or_else(|e| format!("error: {e}"));
            info!("PROBE_QUEST: bags full, no vendor: {gone}");
        }
        if std::mem::take(&mut lua.pending) {
            let eval = |chunk: &str| -> String {
                script.eval(chunk).unwrap_or_else(|e| {
                    warn!("PROBE_QUEST: {chunk}: {e}");
                    String::new()
                })
            };
            let dialog = eval("return BenillaBot.Dialog()");
            let state = eval("return BenillaBot.State()");
            let log = eval("return BenillaBot.Log()");
            lua.readings = Some((dialog, state, log));
        }
    });
}

/// Aim points on a target, as `(up, right)` yards from its feet: the body's middle, the head, low,
/// then either side.
const AIM: [(f32, f32); 5] = [(1.0, 0.0), (1.8, 0.0), (0.5, 0.0), (1.0, 0.5), (1.0, -0.5)];

/// The click in flight: aim the cursor at `entity` (a right-click) or at an interface point `ui`
/// (a left-click; the window's fraction, y down) for a frame, press, release.
#[derive(Resource, Default)]
struct BotClick {
    entity: Option<Entity>,
    ui: Option<Vec2>,
    stage: u8,
    /// Which of [`AIM`] the cursor is on: a target the pick does not hover there is tried at the
    /// next (the camera close behind in a building can put our own body or a wall in the way).
    aim: usize,
    at: Vec2,
}

/// One unit or object in view.
#[derive(Clone, Copy, Debug)]
struct Seen {
    entity: Entity,
    guid: u64,
    kind: Kind,
    entry: u32,
    at: [f32; 3],
    dead: bool,
    lootable: bool,
    tapped_by_other: bool,
    attacking_me: bool,
    npc_flags: u32,
    status: Option<u32>,
}

fn env_f32(name: &str) -> Option<f32> {
    std::env::var(name).ok().and_then(|s| s.trim().parse().ok())
}

/// WoW `(x, y, z)` of a Bevy point: Bevy `(x, y, z)` is WoW `(-y, z, -x)`.
fn wow(p: Vec3) -> [f32; 3] {
    [-p.z, -p.x, p.y]
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

/// The Bevy yaw that faces from `from` toward `to` (WoW `(x, y)`), as the route walker's.
fn yaw_toward(from: [f32; 2], to: [f32; 2]) -> f32 {
    let (bx, bz) = (-(to[1] - from[1]), -(to[0] - from[0]));
    (-bx).atan2(-bz)
}

fn hold(binds: &mut crate::bindings::BindingsState, input: HeldInput, on: bool) {
    binds.apply_device_input(vec![if on {
        BindingInput::Start(input)
    } else {
        BindingInput::Stop(input)
    }]);
}

fn release_all(binds: &mut crate::bindings::BindingsState) {
    for i in [
        HeldInput::MoveForward,
        HeldInput::MoveBackward,
        HeldInput::StrafeLeft,
        HeldInput::StrafeRight,
    ] {
        hold(binds, i, false);
    }
}

/// The world as the bot sees it this tick.
fn look(
    units: &Query<(Entity, &Guid, &NetEntity, &ObjectStore, &GlobalTransform)>,
    me: u64,
    quest_giver: Option<&crate::ui_quest::QuestGiver>,
) -> Vec<Seen> {
    units
        .iter()
        .filter_map(|(entity, guid, net, store, tf)| {
            let kind = match net.kind {
                EntityKind::Unit => Kind::Creature,
                EntityKind::GameObject => Kind::Object,
                _ => return None,
            };
            let f = &store.0;
            let entry = f.object_entry()?;
            let creature = kind == Kind::Creature;
            Some(Seen {
                entity,
                guid: guid.0,
                kind,
                entry,
                at: wow(tf.translation()),
                dead: creature && f.unit_is_dead(),
                lootable: creature && f.unit_lootable(),
                tapped_by_other: creature && f.unit_tapped() && !f.unit_tapped_by_player(),
                attacking_me: creature && f.unit_target() == Some(me) && !f.unit_is_dead(),
                npc_flags: if creature { f.unit_npc_flags() } else { 0 },
                status: quest_giver.and_then(|q| q.status(guid.0)),
            })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn drive_quest_bot(
    mut bot: ResMut<QuestBot>,
    mut click: ResMut<BotClick>,
    time: ProbeClock,
    mut player: ResMut<Player>,
    mut binds: ResMut<crate::bindings::BindingsState>,
    self_player: Query<&Guid, With<SelfPlayer>>,
    units: Query<(Entity, &Guid, &NetEntity, &ObjectStore, &GlobalTransform)>,
    quest_giver: Option<Res<crate::ui_quest::QuestGiver>>,
    mut lua: ResMut<BotLua>,
    idle: Option<ResMut<crate::ui_chat::idle::LastInput>>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(me) = self_player.single() else {
        return;
    };
    let me = me.0;
    if let Some(mut idle) = idle {
        idle.stamp_present(time.elapsed());
    }
    let now = time.elapsed_secs();
    let entered = *bot.entered.get_or_insert(now);
    if now - entered < bot.at {
        return;
    }
    if let Some(limit) = bot.quit_after {
        if now - entered > limit {
            info!("PROBE_QUEST: DONE after {limit:.0} s");
            exit.write(AppExit::Success);
            return;
        }
    }
    let here3 = wow(player.pos);
    let here = [here3[0], here3[1]];

    // Movement every frame: the unstick legs, else toward the goal.
    let goal_at = goal_point(&bot.goal, &units);
    if step_movement(&mut bot, &mut player, &mut binds, here, goal_at, now) {
        return;
    }

    if now < bot.next_think {
        return;
    }
    bot.next_think = now + THINK_SECS;

    // The Lua side's readings from after the last think; ask for the next ones.
    if !lua.set_up {
        lua.pending = true;
        return;
    }
    let Some((dialog, state, log)) = lua.readings.take() else {
        lua.pending = true;
        return;
    };
    lua.pending = true;
    let hp_pool = bot.hp_pool;
    if !dialog.is_empty() {
        info!("PROBE_QUEST: window: {dialog}");
        if dialog.starts_with("accept") {
            bot.accepted += 1;
        } else if let Some(title) = dialog.strip_prefix("reward ") {
            bot.turned_in += 1;
            bot.done.push(title.to_string());
        } else if let Some(at) = dialog.strip_prefix("lootclick ") {
            // A loot row to click with the mouse, at its place in UIParent units (y up).
            let v: Vec<f32> = at.split(' ').filter_map(|v| v.parse().ok()).collect();
            if let [x, y, w, h] = v[..] {
                if click.entity.is_none() && click.ui.is_none() && w > 0.0 && h > 0.0 {
                    click.ui = Some(Vec2::new(x / w, 1.0 - y / h));
                    click.stage = 0;
                    bot.loots += 1;
                }
            }
        }
        // Whatever the goal was a click for is spent: a talk, a loot.
        if matches!(bot.goal, Goal::Use { .. }) {
            if let Some(g) = bot.goal.guid() {
                if dialog.starts_with("loot") {
                    bot.looted.push(g);
                }
                if dialog.ends_with("nothing") || dialog.starts_with("progress-not-done") {
                    bot.aside.insert(g, now + ASIDE_SECS);
                }
            }
            set_goal(&mut bot, Goal::Idle, now, &mut binds);
        }
        return;
    }
    let mut st = state.split('\t').map(|v| v.parse::<u32>().unwrap_or(0));
    let (combat, hp, maxhp, level, attacking, free) = (
        st.next().unwrap_or(0) == 1,
        st.next().unwrap_or(0),
        st.next().unwrap_or(1),
        st.next().unwrap_or(0),
        st.next().unwrap_or(0) == 1,
        st.next().unwrap_or(99),
    );
    // The test's invulnerability: a deep pool, topped up on a level change or when it runs low.
    if level != bot.level || (maxhp > 0 && hp * 3 < maxhp) || maxhp < bot.hp_pool / 2 {
        if level != bot.level && bot.level != 0 {
            info!("PROBE_QUEST: level {level}");
        }
        bot.level = level;
        lua.chat.push(format!(".modify hp {hp_pool}"));
    }
    let log = parse_log(&log);
    if now - bot.last_summary >= 60.0 {
        bot.last_summary = now;
        info!(
            "PROBE_QUEST: summary t={:.0} level={level} kills={} loots={} accepted={} turned_in={} stuck={} quests={}",
            now - entered,
            bot.kills,
            bot.loots,
            bot.accepted,
            bot.turned_in,
            bot.stuck,
            log.len()
        );
    }
    let seen = look(&units, me, quest_giver.as_deref());
    bot.aside.retain(|_, until| *until > now);
    bot.visited.retain(|(_, until)| *until > now);

    // Combat: keep fighting what we fight; the action bar each tick.
    if let Goal::Fight { guid, .. } = bot.goal {
        let target = seen.iter().find(|s| s.guid == guid);
        match target {
            Some(t) if !t.dead => {
                let d = dist2(here, [t.at[0], t.at[1]]);
                // Not yet fighting it: a target of the same kind at least 10 yd nearer takes over
                // (boars wander, and the first choice was the nearest when it was made).
                if !attacking && !t.attacking_me && now - bot.goal_since > 5.0 {
                    if let Some(n) = nearest(&seen, here, |s| {
                        s.guid != t.guid
                            && Some(s.guid) != bot.left_target
                            && s.kind == t.kind
                            && s.entry == t.entry
                            && !s.dead
                            && !s.tapped_by_other
                            && !bot.aside.contains_key(&s.guid)
                    }) {
                        if dist2(here, [n.at[0], n.at[1]]) + 10.0 < d {
                            bot.left_target = Some(t.guid);
                            let why = format!("nearer {}", name_of(&bot.plan, n));
                            let goal = Goal::Fight {
                                entity: n.entity,
                                guid: n.guid,
                                why,
                            };
                            set_goal(&mut bot, goal, now, &mut binds);
                            return;
                        }
                    }
                }
                if d <= MELEE_YD + 1.0 {
                    lua.fight = true;
                }
                // Right-click to attack whenever auto-attack is off (a fresh target, or one that
                // hit us first), once it is close enough to be on screen in front.
                let close = dist2(here, [t.at[0], t.at[1]]) <= 20.0;
                if !attacking && close && now - bot.last_click > 1.5 {
                    request_click(
                        &mut bot,
                        &mut click,
                        t.entity,
                        here,
                        [t.at[0], t.at[1]],
                        now,
                    );
                }
                return;
            }
            Some(t) => {
                bot.kills += 1;
                info!(
                    "PROBE_QUEST: killed {} ({:#x})",
                    name_of(&bot.plan, t),
                    t.guid
                );
                set_goal(&mut bot, Goal::Idle, now, &mut binds);
            }
            None => set_goal(&mut bot, Goal::Idle, now, &mut binds),
        }
    }

    // Something attacking us comes first.
    if let Some(a) = nearest(&seen, here, |s| s.attacking_me && !s.dead) {
        if bot.goal.guid() != Some(a.guid) {
            let why = format!("attacked by {}", name_of(&bot.plan, a));
            set_goal(
                &mut bot,
                Goal::Fight {
                    entity: a.entity,
                    guid: a.guid,
                    why,
                },
                now,
                &mut binds,
            );
        }
        return;
    }

    // A goal in progress: click when there, else keep walking (movement above).
    match bot.goal.clone() {
        Goal::Use {
            entity,
            guid,
            ref why,
        } => {
            let Some(t) = seen.iter().find(|s| s.guid == guid) else {
                set_goal(&mut bot, Goal::Idle, now, &mut binds);
                return;
            };
            // A corpse whose loot is gone (auto loot takes it all on the click, before a loot
            // window is ever read) is done.
            if why.starts_with("loot") && !t.lootable {
                bot.looted.push(guid);
                bot.loots += 1;
                set_goal(&mut bot, Goal::Idle, now, &mut binds);
                return;
            }
            // Still on a route is not there yet: close by in a straight line can be the far side
            // of a wall.
            if bot.route.is_none() && dist2(here, [t.at[0], t.at[1]]) <= USE_YD + 0.5 {
                if bot.clicks >= CLICK_TRIES && bot.escape < 3 {
                    // Clicks that never take are a wall between us: the graph's route there.
                    bot.escape = 3;
                    bot.clicks = 0;
                    if let Some(route) = bot.plan.route(here, [t.at[0], t.at[1]]) {
                        info!("PROBE_QUEST: {guid:#x} never answered; routing around");
                        bot.route = Some(route);
                    }
                } else if bot.clicks >= CLICK_TRIES {
                    warn!("PROBE_QUEST: {guid:#x} never answered {CLICK_TRIES} clicks; set aside");
                    bot.aside.insert(guid, now + ASIDE_SECS);
                    set_goal(&mut bot, Goal::Idle, now, &mut binds);
                } else if now - bot.last_click > 1.5 {
                    request_click(&mut bot, &mut click, entity, here, [t.at[0], t.at[1]], now);
                }
            }
            return;
        }
        Goal::Walk { to, .. } => {
            if dist2(here, to) > USE_YD {
                // Still on the way: anything worth fighting or talking to that came into view
                // on the way takes over from the walk.
                let next = choose(&bot, &seen, &log, here, level, free);
                if matches!(next, Goal::Fight { .. } | Goal::Use { .. }) {
                    set_goal(&mut bot, next, now, &mut binds);
                }
                return;
            }
            bot.visited.push((to, now + ASIDE_SECS));
            set_goal(&mut bot, Goal::Idle, now, &mut binds);
        }
        Goal::Fight { .. } | Goal::Idle => {}
    }

    let next = choose(&bot, &seen, &log, here, level, free);
    // Bags full and no vendor to reach: make room.
    if free == 0
        && !matches!(next, Goal::Use { ref why, .. } | Goal::Walk { ref why, .. } if why.starts_with("sell"))
    {
        lua.destroy = true;
    }
    if next != bot.goal {
        set_goal(&mut bot, next, now, &mut binds);
    }

    let _ = combat;
}

fn name_of(plan: &Plan, s: &Seen) -> String {
    plan.names
        .get(&(s.kind, s.entry))
        .map_or_else(|| format!("entry {}", s.entry), |(n, _)| n.clone())
}

fn nearest(seen: &[Seen], here: [f32; 2], f: impl Fn(&Seen) -> bool) -> Option<&Seen> {
    seen.iter()
        .filter(|s| f(s) && dist2(here, [s.at[0], s.at[1]]) <= SEEK_YD)
        .min_by(|a, b| dist2(here, [a.at[0], a.at[1]]).total_cmp(&dist2(here, [b.at[0], b.at[1]])))
}

/// The next goal, in a player's order: loot what we killed, turn in, take offers, work the
/// objectives, else hunt the nearest thing the zone's quests want.
fn choose(
    bot: &QuestBot,
    seen: &[Seen],
    log: &[LogQuest],
    here: [f32; 2],
    level: u32,
    bag_free: u32,
) -> Goal {
    let free = |s: &Seen| !bot.aside.contains_key(&s.guid);
    // 0. Bags nearly full: a vendor, those that open their shop at once first (no gossip bit).
    if bag_free < 2 {
        let vendors = |gossip: bool| -> Vec<(Kind, u32)> {
            bot.plan
                .names
                .iter()
                .filter(|(_, (_, f))| f & 0x4 != 0 && (f & 0x1 != 0) == gossip)
                .map(|(&k, _)| k)
                .collect()
        };
        for gossip in [false, true] {
            if let Some(g) = goal_for_entries(
                bot,
                seen,
                here,
                &vendors(gossip),
                |s| !s.dead,
                "vendor",
                "sell",
            ) {
                return g;
            }
        }
    }
    // 1. Our kills' corpses with loot.
    if let Some(c) = nearest(seen, here, |s| {
        s.dead && s.lootable && !bot.looted.contains(&s.guid) && free(s)
    }) {
        return Goal::Use {
            entity: c.entity,
            guid: c.guid,
            why: format!("loot {}", name_of(&bot.plan, c)),
        };
    }
    // 2. A complete quest: its turn-in, in view or at its spawn.
    for q in log.iter().filter(|q| q.complete) {
        let Some(pq) = bot.plan.quest_titled(&q.title) else {
            continue;
        };
        if let Some(g) =
            goal_for_entries(bot, seen, here, &pq.enders, |_| true, &q.title, "turn in")
        {
            return g;
        }
    }
    // 3. A giver showing an offer or a reward mark in view.
    if let Some(n) = nearest(seen, here, |s| {
        free(s)
            && !s.dead
            && s.status
                .is_some_and(|st| st == STATUS_AVAILABLE || STATUS_REWARD.contains(&st))
    }) {
        return Goal::Use {
            entity: n.entity,
            guid: n.guid,
            why: format!("talk to {}", name_of(&bot.plan, n)),
        };
    }
    // 4. Objectives: kill or use what a monster/object objective names; kill or open what drops an
    //    item objective's item.
    for q in log.iter().filter(|q| !q.complete) {
        for (ty, done, name) in &q.objectives {
            if *done {
                continue;
            }
            let wanted: Vec<(Kind, u32)> = match ty.as_str() {
                "item" => bot
                    .plan
                    .quest_titled(&q.title)
                    .and_then(|pq| pq.items.iter().find(|(_, _, n)| n == name))
                    .and_then(|(item, _, _)| bot.plan.droppers.get(item).cloned())
                    .unwrap_or_default(),
                _ => bot.plan.entries_named(name),
            };
            if wanted.is_empty() {
                continue;
            }
            if let Some(g) =
                goal_for_entries(bot, seen, here, &wanted, |s| !s.dead, name, "objective")
            {
                return g;
            }
        }
    }
    // 5. Givers out of view with a quest we could take: walk to the nearest unvisited one. A quest
    //    we could take: not in the log, not turned in, our level, its earlier quest turned in.
    let title_of = |id: u32| {
        bot.plan
            .quests
            .iter()
            .find(|(q, _)| *q == id)
            .map(|(_, q)| q.title.as_str())
    };
    let open = |q: &PlanQuest| {
        !log.iter().any(|l| l.title == q.title)
            && !bot.done.contains(&q.title)
            && q.min_level <= level.max(1)
            && (q.prev == 0 || title_of(q.prev).is_some_and(|t| bot.done.iter().any(|d| d == t)))
    };
    let mut giver_entries: Vec<(Kind, u32)> = bot
        .plan
        .quests
        .iter()
        .filter(|(_, q)| open(q))
        .flat_map(|(_, q)| q.givers.iter().copied())
        .collect();
    giver_entries.sort_by_key(|&(k, e)| (k == Kind::Object, e));
    giver_entries.dedup();
    let in_view: Vec<(Kind, u32)> = seen.iter().map(|s| (s.kind, s.entry)).collect();
    let out_of_view: Vec<(Kind, u32)> = giver_entries
        .into_iter()
        .filter(|k| !in_view.contains(k))
        .collect();
    if let Some(to) = nearest_spawn(bot, here, &out_of_view) {
        return Goal::Walk {
            to,
            why: "a quest giver out of view".into(),
        };
    }
    // 6. Nothing to do for a quest: hunt the nearest hostile creature near our level.
    let _ = level;
    if let Some(m) = nearest(seen, here, |s| {
        s.kind == Kind::Creature
            && !s.dead
            && s.npc_flags == 0
            && !s.tapped_by_other
            && free(s)
            && bot
                .plan
                .names
                .get(&(s.kind, s.entry))
                .is_some_and(|(_, f)| *f == 0)
    }) {
        return Goal::Fight {
            entity: m.entity,
            guid: m.guid,
            why: format!("grind {}", name_of(&bot.plan, m)),
        };
    }
    Goal::Idle
}

/// The nearest live unit or object of `entries` in view (fight a creature that is no NPC, use an
/// object or talk to an NPC), else a walk to the nearest unvisited spawn of them.
fn goal_for_entries(
    bot: &QuestBot,
    seen: &[Seen],
    here: [f32; 2],
    entries: &[(Kind, u32)],
    ok: impl Fn(&Seen) -> bool,
    what: &str,
    why: &str,
) -> Option<Goal> {
    let target = nearest(seen, here, |s| {
        entries.contains(&(s.kind, s.entry))
            && ok(s)
            && !s.tapped_by_other
            && !bot.aside.contains_key(&s.guid)
    });
    if let Some(t) = target {
        let label = format!("{why}: {what}");
        let hostile = t.kind == Kind::Creature && t.npc_flags == 0;
        return Some(if hostile {
            Goal::Fight {
                entity: t.entity,
                guid: t.guid,
                why: label,
            }
        } else {
            Goal::Use {
                entity: t.entity,
                guid: t.guid,
                why: label,
            }
        });
    }
    nearest_spawn(bot, here, entries).map(|to| Goal::Walk {
        to,
        why: format!("{why}: {what}"),
    })
}

fn nearest_spawn(bot: &QuestBot, here: [f32; 2], entries: &[(Kind, u32)]) -> Option<[f32; 2]> {
    entries
        .iter()
        .filter_map(|k| bot.plan.spawns.get(k))
        .flatten()
        .map(|p| [p[0], p[1]])
        .filter(|p| !bot.visited.iter().any(|(v, _)| dist2(*v, *p) < 5.0))
        .filter(|p| dist2(here, *p) > USE_YD)
        .min_by(|a, b| dist2(here, *a).total_cmp(&dist2(here, *b)))
}

fn set_goal(bot: &mut QuestBot, goal: Goal, now: f32, binds: &mut crate::bindings::BindingsState) {
    if bot.goal != goal {
        info!("PROBE_QUEST: {}", goal.label());
    }
    bot.goal = goal;
    bot.goal_since = now;
    bot.clicks = 0;
    bot.tries = 0;
    bot.unstick = Unstick::None;
    bot.route = None;
    bot.escape = 0;
    bot.moved_from = None;
    if bot.walking {
        release_all(binds);
        bot.walking = false;
    }
}

/// Where the goal is, in WoW `(x, y)`, and how near counts as there.
fn goal_point(
    goal: &Goal,
    units: &Query<(Entity, &Guid, &NetEntity, &ObjectStore, &GlobalTransform)>,
) -> Option<([f32; 2], f32)> {
    match goal {
        Goal::Idle => None,
        Goal::Walk { to, .. } => Some((*to, USE_YD)),
        Goal::Use { entity, .. } => units.get(*entity).ok().map(|(.., tf)| {
            let p = wow(tf.translation());
            ([p[0], p[1]], USE_YD)
        }),
        Goal::Fight { entity, .. } => units.get(*entity).ok().map(|(.., tf)| {
            let p = wow(tf.translation());
            ([p[0], p[1]], MELEE_YD)
        }),
    }
}

/// The length of a walk from `from` along `route`.
fn path_len(from: [f32; 2], route: &[[f32; 2]]) -> f32 {
    let mut at = from;
    let mut len = 0.0;
    for &p in route {
        len += dist2(at, p);
        at = p;
    }
    len
}

/// The way back along the trail: its points newest first, from the first one 2 yd off, up to 12
/// (about 24 yd back), then the goal.
fn retrace(trail: &[[f32; 2]], here: [f32; 2], to: [f32; 2]) -> Vec<[f32; 2]> {
    let mut back: Vec<[f32; 2]> = trail
        .iter()
        .rev()
        .skip_while(|p| dist2(**p, here) < 2.0)
        .take(12)
        .copied()
        .collect();
    back.push(to);
    back
}

/// Moves toward the goal, or runs an unstick leg; true while an unstick leg holds the frame.
fn step_movement(
    bot: &mut QuestBot,
    player: &mut Player,
    binds: &mut crate::bindings::BindingsState,
    here: [f32; 2],
    goal: Option<([f32; 2], f32)>,
    now: f32,
) -> bool {
    match bot.unstick {
        Unstick::Back { until } if now < until => {
            hold(binds, HeldInput::MoveBackward, true);
            return true;
        }
        Unstick::Back { .. } => {
            hold(binds, HeldInput::MoveBackward, false);
            let left = bot.tries.is_multiple_of(2);
            bot.unstick = Unstick::Side {
                until: now + UNSTICK_LEG,
                left,
            };
            return true;
        }
        Unstick::Side { until, left } if now < until => {
            let side = if left {
                HeldInput::StrafeLeft
            } else {
                HeldInput::StrafeRight
            };
            hold(binds, side, true);
            return true;
        }
        Unstick::Side { left, .. } => {
            let side = if left {
                HeldInput::StrafeLeft
            } else {
                HeldInput::StrafeRight
            };
            hold(binds, side, false);
            let bearing = goal.map_or(player.facing(), |(to, _)| yaw_toward(here, to));
            let (yaw, secs) = detour(bot.tries, bearing);
            bot.unstick = Unstick::Detour {
                until: now + secs,
                yaw,
            };
            return true;
        }
        Unstick::Detour { until, yaw } if now < until => {
            let turn = (yaw - player.facing() + std::f32::consts::PI)
                .rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            player.turn_aim(turn);
            hold(binds, HeldInput::MoveForward, true);
            return true;
        }
        Unstick::Detour { .. } => {
            hold(binds, HeldInput::MoveForward, false);
            bot.unstick = Unstick::None;
            bot.moved_from = None;
        }
        Unstick::None => {}
    }
    let Some((to, near)) = goal else {
        if bot.walking {
            hold(binds, HeldInput::MoveForward, false);
            bot.walking = false;
        }
        return false;
    };
    let d = dist2(here, to);
    if bot.trail.last().is_none_or(|l| dist2(*l, here) >= 2.0) {
        bot.trail.push(here);
        if bot.trail.len() > 400 {
            bot.trail.remove(0);
        }
    }
    // Ahead of time, to an NPC, an object or a spawn: the walk graph's route, the navmesh's way
    // through doors and around buildings (on average ~1.2x the straight line), when there is more
    // than a short walk. Not for a fight: mobs stand in the open and move, and the stuck handling
    // covers a wall.
    if bot.escape == 0 && !matches!(bot.goal, Goal::Fight { .. }) {
        bot.escape = 1;
        if d > 15.0 {
            if let Some(route) = bot.plan.route(here, to) {
                info!(
                    "PROBE_QUEST: routing via {} waypoints ({:.0} yd, {d:.0} straight)",
                    route.len(),
                    path_len(here, &route)
                );
                bot.route = Some(route);
            }
        }
    }
    // On a route, steer for its next waypoint; drop each as it is reached. Its last point is where
    // the goal was when the route was made: from there, steer for where it is now.
    let mut steer = to;
    if let Some(route) = bot.route.as_mut() {
        while !route.is_empty() && dist2(here, route[0]) < 1.0 {
            route.remove(0);
        }
        if route.len() > 1 {
            steer = route[0];
        } else {
            bot.route = None;
        }
    }
    let want = yaw_toward(here, steer);
    let turn = (want - player.facing() + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU)
        - std::f32::consts::PI;
    player.turn_aim(turn);
    if d <= near {
        // Within reach of the goal: any route there is done.
        bot.route = None;
        if bot.walking {
            hold(binds, HeldInput::MoveForward, false);
            bot.walking = false;
        }
        bot.moved_from = None;
        return false;
    }
    hold(binds, HeldInput::MoveForward, true);
    bot.walking = true;
    // The stuck test: compare with where we were a window ago.
    match bot.moved_from {
        None => bot.moved_from = Some((here, now)),
        Some((from, since)) if now - since >= STUCK_WINDOW => {
            if dist2(from, here) < STUCK_YD {
                if let Some(g) = bot.goal.guid() {
                    let n = bot.stuck_on.entry(g).or_insert(0);
                    *n += 1;
                    if *n > UNSTICK_TRIES {
                        warn!("PROBE_QUEST: stuck {n}x on {g:#x} across goals; set aside");
                        bot.aside.insert(g, now + ASIDE_SECS);
                        bot.stuck_on.remove(&g);
                        bot.goal = Goal::Idle;
                        bot.route = None;
                        release_all(binds);
                        bot.walking = false;
                        bot.moved_from = None;
                        return false;
                    }
                }
                // On a route: the waypoint we cannot reach (an NPC may stand on it) is skipped.
                if let Some(route) = bot.route.as_mut() {
                    if route.len() > 2 {
                        route.remove(0);
                        info!(
                            "PROBE_QUEST: stuck at ({:.1}, {:.1}); skipping a waypoint",
                            here[0], here[1]
                        );
                        bot.moved_from = None;
                        return false;
                    }
                }
                // First stuck on this goal: walk back the way we came (out the door we came in by);
                // then the walk graph's route; then the detours.
                if bot.escape < 2 {
                    bot.escape = 2;
                    let back = retrace(&bot.trail, here, to);
                    if back.len() > 2 {
                        info!(
                            "PROBE_QUEST: stuck at ({:.1}, {:.1}); retracing {} steps",
                            here[0],
                            here[1],
                            back.len() - 1
                        );
                        bot.route = Some(back);
                        bot.moved_from = None;
                        return false;
                    }
                }
                if bot.escape < 3 {
                    bot.escape = 3;
                    if let Some(route) = bot.plan.route(here, to) {
                        info!(
                            "PROBE_QUEST: stuck at ({:.1}, {:.1}); routing via {} waypoints",
                            here[0],
                            here[1],
                            route.len()
                        );
                        bot.route = Some(route);
                        bot.moved_from = None;
                        return false;
                    }
                }
                bot.tries += 1;
                bot.stuck += 1;
                if bot.tries > UNSTICK_TRIES {
                    warn!(
                        "PROBE_QUEST: stuck {UNSTICK_TRIES}x on {}; choosing another",
                        bot.goal.label()
                    );
                    match bot.goal.guid() {
                        Some(g) => {
                            bot.aside.insert(g, now + ASIDE_SECS);
                        }
                        None => bot.visited.push((to, now + ASIDE_SECS)),
                    }
                    bot.goal = Goal::Idle;
                    release_all(binds);
                    bot.walking = false;
                    bot.tries = 0;
                    bot.moved_from = None;
                    return false;
                }
                info!(
                    "PROBE_QUEST: stuck at ({:.1}, {:.1}), try {}: back up, side-step, detour",
                    here[0], here[1], bot.tries
                );
                hold(binds, HeldInput::MoveForward, false);
                bot.walking = false;
                bot.unstick = Unstick::Back {
                    until: now + UNSTICK_LEG,
                };
                return true;
            }
            bot.moved_from = Some((here, now));
        }
        Some(_) => {}
    }
    false
}

fn request_click(
    bot: &mut QuestBot,
    click: &mut BotClick,
    entity: Entity,
    _here: [f32; 2],
    _at: [f32; 2],
    now: f32,
) {
    if click.entity.is_some() || click.ui.is_some() {
        return;
    }
    click.entity = Some(entity);
    click.stage = 0;
    click.aim = 0;
    bot.clicks += 1;
    bot.last_click = now;
}

/// The click in flight, before the input pass: the cursor onto the target for a frame (so the
/// pick sees it there), then the right button down, then up; the same messages a mouse writes.
#[allow(clippy::too_many_arguments)]
fn press_bot_click(
    mut click: ResMut<BotClick>,
    mut windows: Query<(Entity, &mut Window)>,
    camera: Query<(&Camera, &GlobalTransform), With<benilla_world::view::WorldCamera>>,
    targets: Query<&GlobalTransform>,
    hovered: Option<Res<crate::target::Hovered>>,
    hovered_object: Option<Res<crate::target::HoveredObject>>,
    mut moved: MessageWriter<CursorMoved>,
    mut buttons: MessageWriter<MouseButtonInput>,
    mut stream: MessageWriter<WindowEvent>,
) {
    if click.entity.is_none() && click.ui.is_none() {
        return;
    }
    let Some((window_entity, mut window)) = windows.iter_mut().next() else {
        return;
    };
    let size = Vec2::new(window.width(), window.height());
    let button = if click.ui.is_some() {
        MouseButton::Left
    } else {
        MouseButton::Right
    };
    let mut cursor = |at: Vec2| {
        window
            .bypass_change_detection()
            .set_cursor_position(Some(at));
        let ev = CursorMoved {
            window: window_entity,
            position: at,
            delta: None,
        };
        moved.write(ev.clone());
        stream.write(WindowEvent::CursorMoved(ev));
    };
    let press = |buttons: &mut MessageWriter<MouseButtonInput>,
                 stream: &mut MessageWriter<WindowEvent>| {
        let ev = MouseButtonInput {
            button,
            state: ButtonState::Pressed,
            window: window_entity,
        };
        buttons.write(ev);
        stream.write(WindowEvent::MouseButtonInput(ev));
    };
    match click.stage {
        0 | 1 if click.ui.is_some() => {
            let p = click.ui.expect("checked") * size;
            click.at = p;
            cursor(p);
            if click.stage == 1 {
                press(&mut buttons, &mut stream);
            }
            click.stage += 1;
        }
        0 | 1 => {
            let entity = click.entity.expect("checked above");
            // From the second frame: does the pick hover the target where the cursor is?
            if click.stage == 1 {
                let on = hovered
                    .as_ref()
                    .is_some_and(|h| h.target == Some(entity) || h.corpse == Some(entity))
                    || hovered_object
                        .as_ref()
                        .is_some_and(|h| h.target == Some(entity));
                if on {
                    press(&mut buttons, &mut stream);
                    click.stage = 2;
                    return;
                }
                click.aim += 1;
                if click.aim >= AIM.len() {
                    warn!("PROBE_QUEST: no aim point hovers the target; no click");
                    click.entity = None;
                    return;
                }
            }
            let (Ok(tf), Ok((cam, cam_tf))) = (targets.get(entity), camera.single()) else {
                click.entity = None;
                return;
            };
            let (up, right) = AIM[click.aim];
            let world = tf.translation() + Vec3::Y * up + cam_tf.right() * right;
            let Ok(p) = cam.world_to_viewport(cam_tf, world) else {
                warn!("PROBE_QUEST: target off screen; no click");
                click.entity = None;
                return;
            };
            click.at = p;
            cursor(p);
            click.stage = 1;
        }
        _ => {
            let ev = MouseButtonInput {
                button,
                state: ButtonState::Released,
                window: window_entity,
            };
            buttons.write(ev);
            stream.write(WindowEvent::MouseButtonInput(ev));
            click.entity = None;
            click.ui = None;
        }
    }
}

/// Registers the bot when `WOW_PROBE_QUEST` names a readable plan; inert otherwise.
pub(crate) struct ProbeQuestPlugin;

impl Plugin for ProbeQuestPlugin {
    fn build(&self, app: &mut App) {
        let Ok(path) = std::env::var("WOW_PROBE_QUEST") else {
            return;
        };
        // A relative path not found from the working folder is the state folder's (on the iPad,
        // the app's Documents/benilla-config, where the workbench copies the plan).
        let path = {
            let p = std::path::PathBuf::from(&path);
            match (p.is_relative() && !p.exists(), crate::local_state::home()) {
                (true, Some(home)) => home.join(p).display().to_string(),
                _ => path,
            }
        };
        let plan = match std::fs::read_to_string(&path) {
            Ok(text) => Plan::parse(&text),
            Err(e) => {
                warn!("PROBE_QUEST: cannot read the plan {path}: {e}");
                return;
            }
        };
        info!(
            "PROBE_QUEST: plan {path}: {} quests, {} named entries, {} spawn sets",
            plan.quests.len(),
            plan.names.len(),
            plan.spawns.len()
        );
        app.insert_resource(QuestBot {
            plan,
            at: env_f32("WOW_PROBE_QUEST_AT").unwrap_or(10.0),
            hp_pool: env_f32("WOW_PROBE_QUEST_HP").unwrap_or(5000.0) as u32,
            quit_after: env_f32("WOW_PROBE_QUEST_SECS"),
            entered: None,
            next_think: 0.0,
            goal: Goal::Idle,
            goal_since: 0.0,
            clicks: 0,
            last_click: 0.0,
            tries: 0,
            unstick: Unstick::None,
            route: None,
            trail: Vec::new(),
            escape: 0,
            moved_from: None,
            aside: HashMap::new(),
            visited: Vec::new(),
            looted: Vec::new(),
            stuck_on: HashMap::new(),
            left_target: None,
            done: Vec::new(),
            walking: false,
            level: 0,
            kills: 0,
            loots: 0,
            accepted: 0,
            turned_in: 0,
            stuck: 0,
            last_summary: 0.0,
        })
        .insert_resource(BotLua {
            hp_pool: env_f32("WOW_PROBE_QUEST_HP").unwrap_or(5000.0) as u32,
            set_up: false,
            pending: false,
            fight: false,
            destroy: false,
            chat: Vec::new(),
            readings: None,
        })
        .init_resource::<BotClick>()
        .add_systems(PreUpdate, press_bot_click.before(bevy::input::InputSystems))
        .add_systems(
            Update,
            drive_quest_bot
                .in_set(WorldStage::Input)
                .before(crate::player::PlayerControlSet)
                .in_set(crate::client_state::InWorldGated),
        )
        .add_systems(
            Update,
            run_bot_lua
                .after(drive_quest_bot)
                .in_set(crate::client_state::InWorldGated),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "Q\t788\t1\t0\tCutting Teeth\n\
QG\t788\tc\t3143\n\
QT\t788\tc\t3143\n\
QK\t788\tc\t3098\t10\n\
Q\t789\t1\t788\tSting of the Scorpid\n\
QI\t789\t4862\t10\tScorpid Worker Tail\n\
D\t4862\tc\t3124\n\
N\tc\t3098\t0\tMottled Boar\n\
N\tc\t3143\t3\tGornek\n\
S\tc\t3098\t-500.0\t-4300.0\t40.0\n\
S\tc\t3098\t-520.0\t-4310.0\t41.0\n";

    #[test]
    fn the_plan_parses_quests_objectives_drops_names_and_spawns() {
        let p = Plan::parse(PLAN);
        assert_eq!(p.quests.len(), 2);
        let q = p.quest_titled("Cutting Teeth").unwrap();
        assert_eq!(q.givers, vec![(Kind::Creature, 3143)]);
        assert_eq!(q.kills, vec![(Kind::Creature, 3098, 10)]);
        let q = p.quest_titled("Sting of the Scorpid").unwrap();
        assert_eq!(q.items, vec![(4862, 10, "Scorpid Worker Tail".to_string())]);
        assert_eq!(p.droppers[&4862], vec![(Kind::Creature, 3124)]);
        assert_eq!(
            p.entries_named("mottled boar"),
            vec![(Kind::Creature, 3098)]
        );
        assert_eq!(p.spawns[&(Kind::Creature, 3098)].len(), 2);
    }

    #[test]
    fn a_route_follows_the_graph_around_a_wall() {
        // A square of four nodes, 0-1-2-3, with no edge 0-3: from near 0 to near 3 goes the long way.
        let p = Plan::parse(
            "W\t0\t0\t0\t0\nW\t1\t0\t10\t0\nW\t2\t10\t10\t0\nW\t3\t10\t0\t0\n\
E\t0\t1\nE\t1\t2\nE\t2\t3\n",
        );
        let r = p.route([0.5, 0.0], [10.5, 0.0]).unwrap();
        assert_eq!(
            r,
            vec![
                [0.0, 0.0],
                [0.0, 10.0],
                [10.0, 10.0],
                [10.0, 0.0],
                [10.5, 0.0]
            ]
        );
        assert!(Plan::default().route([0.0; 2], [1.0; 2]).is_none());
    }

    #[test]
    fn a_retrace_walks_the_trail_back_newest_first_then_the_goal() {
        let trail: Vec<[f32; 2]> = (0..20).map(|i| [i as f32 * 2.0, 0.0]).collect();
        let here = [38.5, 0.0];
        let back = retrace(&trail, here, [50.0, 50.0]);
        assert_eq!(
            back.first(),
            Some(&[36.0, 0.0]),
            "skips the points underfoot"
        );
        assert_eq!(back.len(), 13, "twelve steps back, then the goal");
        assert_eq!(back.last(), Some(&[50.0, 50.0]));
    }

    #[test]
    fn a_leaderboard_line_names_its_subject() {
        assert_eq!(objective_name("Mottled Boar slain: 3/10"), "Mottled Boar");
        assert_eq!(
            objective_name("Scorpid Worker Tail: 2/10"),
            "Scorpid Worker Tail"
        );
        assert_eq!(objective_name("Speak to Gornek"), "Speak to Gornek");
    }

    #[test]
    fn the_log_parses_titles_completion_and_objectives() {
        let log = parse_log(
            "Cutting Teeth\t0\tmonster|0|Mottled Boar slain: 3/10\n\
Your Place In The World\t1\n",
        );
        assert_eq!(log.len(), 2);
        assert_eq!(
            log[0].objectives,
            vec![("monster".to_string(), false, "Mottled Boar".to_string())]
        );
        assert!(log[1].complete && log[1].objectives.is_empty());
    }

    #[test]
    fn detours_alternate_sides_widen_and_lengthen() {
        let (a, t1) = detour(1, 0.0);
        let (b, t2) = detour(2, 0.0);
        let (c, _) = detour(3, 0.0);
        assert!(a > 0.0 && b < 0.0 && c > a, "{a} {b} {c}");
        assert!(t2 > t1);
        assert_eq!(
            detour(9, 0.0).0,
            detour(6, 0.0).0,
            "past the table, the widest"
        );
    }

    #[test]
    fn wow_axes_round_trip_the_route_walkers() {
        // Bevy (x, y, z) = WoW (-y, z, -x).
        let w = wow(Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(w, [-3.0, -1.0, 2.0]);
    }
}
