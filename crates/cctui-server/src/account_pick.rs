//! Ranking the candidate accounts an `auto` spawn may bind to.
//!
//! `POST /spawn` with `auto_account` asks the server to choose between the
//! caller's accounts instead of refusing to guess. "Best" here means *most
//! allocation left for the session about to run*, which is not the same as
//! "least used": an account can sit at 0% of its 5h window and still be at 100%
//! of its weekly one, so it would rate-limit on the first request. An account is
//! therefore judged on its **narrowest** window among those that apply, never
//! on its idlest one.
//!
//! Three rules shape which windows apply:
//!
//!   * model-scoped windows count only for the model being launched — an
//!     account whose weekly Fable budget is spent is still fine for an Opus
//!     session;
//!   * the account's own configured soft limits count, so `auto` never elects
//!     an account the gateway would 429 moments later;
//!   * dollar windows are excluded from the margin (a percent of a USD budget
//!     means nothing), though their caps still block through the soft limit.
//!
//! A margin alone is not room, though: 35% of a weekly window that resets in
//! 18 hours can be spent far faster than 35% of one that resets in two hours
//! would ever need to be. The `headroom` score is therefore a **sustainable
//! rate**: each applicable percent window contributes `margin / hours until its
//! reset` (hours floored at [`MIN_HOURS_TO_RESET`]; a window with no reset time
//! contributes its raw margin), the account's score is the lowest of those
//! rates, divided by its pace penalty, then shared with the sessions already in
//! flight on it (`score / (1 + in_flight × RESERVATION)`). The last step is what
//! walks a wave of spawns across the pool: usage read between two spawns has
//! not moved yet, so without it every spawn of the wave lands on the same
//! account and they all hit its wall in the same second.
//!
//! An account that opted into pace limits and is burning faster than its linear
//! budget has its score further discounted by that burn rate, so equal room
//! goes to the calmer account.
//!
//! Pools choose between these two rankers with their `strategy`. Moving a live
//! session to a sibling when its account refuses is also a pool setting
//! (`failover` on the pool itself); `CCTUI_GATEWAY_FAILOVER=1` only governs the
//! explicit `account_redirects` rules, never in-pool moves.
//!
//! This module is pure: the caller does the DB reads and the usage fetches, so
//! every rule above is unit-testable without a database or a network.

use chrono::{DateTime, Utc};

use crate::soft_limit::{Decision, SoftLimits, UsageWindow, evaluate_soft_limit, window_applies};

/// One account the spawn could bind to, as the ranker sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Account name — what a spawn binds and what the error/toast shows.
    pub name: String,
    /// Normalized usage windows for the credential that will actually serve.
    /// Empty when usage could not be read; see `usage_known`.
    pub windows: Vec<UsageWindow>,
    /// The account's configured caps.
    pub limits: SoftLimits,
    /// Whether `windows` reflects a real reading. An account whose usage
    /// endpoint failed has no windows, which must NOT be read as "wide open"
    /// nor as "exhausted" — only as unknown.
    pub usage_known: bool,
    /// Sessions already bound to (and served by) this account that are still
    /// live, or were bound too recently to show up in its usage yet. Counted
    /// by the caller; see [`RESERVATION`].
    pub in_flight: u32,
}

/// Why one candidate is out of the running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub name: String,
    /// Human-readable cause, naming the window and its reset.
    pub reason: String,
}

/// The outcome of ranking.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    /// Bind this account. `headroom_pct` is the raw margin of the window that
    /// decided its score, `score` the sustainable rate it was ranked on and
    /// `resets_at` that window's reset. All `None` when the pick was made
    /// without usage data.
    Chosen {
        name: String,
        headroom_pct: Option<f64>,
        score: Option<f64>,
        resets_at: Option<DateTime<Utc>>,
    },
    /// Every candidate was readable AND out of allocation.
    Exhausted(Vec<Blocked>),
    /// No candidate at all.
    None,
}

/// Floor, in hours, on the time left before a window resets when turning its
/// margin into a rate: a window resetting in the next minutes (or already past
/// its reset, on a stale reading) must rank as very roomy, not divide by zero.
pub const MIN_HOURS_TO_RESET: f64 = 0.25;

/// Share of an account's sustainable rate each session already in flight on it
/// is assumed to take. `1.0` means a new session gets a fair share of the rate:
/// with `n` sessions running, it can count on `score / (n + 1)`. Smaller values
/// under-count what a running session burns: at `0.15`, the incident this was
/// written for (16 spawns in three hours, see the tests) still piles every
/// spawn onto the same account.
pub const RESERVATION: f64 = 1.0;

/// Percent windows carry a meaningful margin; dollar ones do not.
const fn is_percent_window(window: &UsageWindow) -> bool {
    window.amount_usd.is_none()
}

/// How much room a candidate has, once time and load are weighed in.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Standing {
    /// What the candidate is ranked on (higher is better).
    score: f64,
    /// Raw margin of the window that set the score.
    headroom_pct: f64,
    /// That window's reset.
    resets_at: Option<DateTime<Utc>>,
}

/// Margin per hour left in `window`: the rate at which it can be spent without
/// hitting its wall before it resets. A window that never resets keeps its raw
/// margin.
fn sustainable_rate(window: &UsageWindow, now: DateTime<Utc>) -> f64 {
    let margin = 100.0 - window.utilization;
    window.resets_at.map_or(margin, |resets_at| {
        let hours = (resets_at - now).num_seconds() as f64 / 3600.0;
        margin / hours.max(MIN_HOURS_TO_RESET)
    })
}

/// The tightest applicable percent window (lowest sustainable rate) with that
/// rate, or `None` when there is none to measure.
fn tightest_window<'w>(
    windows: &[&'w UsageWindow],
    now: DateTime<Utc>,
) -> Option<(f64, &'w UsageWindow)> {
    windows
        .iter()
        .filter(|w| is_percent_window(w))
        .map(|w| (sustainable_rate(w, now), *w))
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
}

/// A window at or past 100% is spent regardless of any configured cap.
fn spent_window<'w>(windows: &[&'w UsageWindow]) -> Option<&'w UsageWindow> {
    windows.iter().find(|w| is_percent_window(w) && w.utilization >= 100.0).copied()
}

fn resets_phrase(window: &UsageWindow, now: DateTime<Utc>) -> String {
    let Some(resets_at) = window.resets_at else { return String::new() };
    let mins = (resets_at - now).num_minutes();
    if mins <= 0 {
        return String::new();
    }
    if mins < 60 {
        format!(", resets in {mins}m")
    } else {
        format!(", resets in {}h", (mins + 59) / 60)
    }
}

/// Rank `candidates` and pick one.
///
/// Ordering: accounts with a known, positive margin first (highest score wins,
/// ties broken by name so the choice is deterministic); then accounts whose
/// usage could not be read, least loaded first, then by name. An unreadable
/// account ranks behind every readable one with room — we have positive
/// evidence for the latter and none for the former — but it stays eligible, so
/// a flaky usage endpoint degrades the choice instead of blocking the launch.
///
/// [`Pick::Exhausted`] is returned only when every candidate was read AND every
/// one is out. "Usage unavailable" must never masquerade as "you are out of
/// allocation".
pub fn pick_account(candidates: &[Candidate], model: Option<&str>, now: DateTime<Utc>) -> Pick {
    if candidates.is_empty() {
        return Pick::None;
    }

    let mut ranked: Vec<(Standing, &str)> = Vec::new();
    let mut unknown: Vec<(u32, &str)> = Vec::new();
    let mut blocked: Vec<Blocked> = Vec::new();

    for candidate in candidates {
        match availability(candidate, model, now) {
            Err(reason) => blocked.push(reason),
            // Readable, but nothing measurable (no percent window at all):
            // eligible, yet with no margin to compare, so it queues with the
            // unknowns rather than outranking a measured account.
            Ok(None) => unknown.push((candidate.in_flight, &candidate.name)),
            Ok(Some(standing)) => ranked.push((standing, &candidate.name)),
        }
    }

    // Highest score first; name ascending breaks ties so repeated spawns under
    // identical usage and load always land on the same account.
    ranked.sort_by(|a, b| {
        b.0.score.partial_cmp(&a.0.score).unwrap_or(std::cmp::Ordering::Equal).then(a.1.cmp(b.1))
    });
    if let Some((standing, name)) = ranked.first() {
        return Pick::Chosen {
            name: (*name).to_owned(),
            headroom_pct: Some(standing.headroom_pct),
            score: Some(standing.score),
            resets_at: standing.resets_at,
        };
    }
    // Nothing measured to compare: spread by load, then by name.
    unknown.sort_unstable();
    if let Some((_, name)) = unknown.first() {
        return Pick::Chosen {
            name: (*name).to_owned(),
            headroom_pct: None,
            score: None,
            resets_at: None,
        };
    }
    blocked.sort_by(|a, b| a.name.cmp(&b.name));
    Pick::Exhausted(blocked)
}

/// One candidate's standing: `Err` when it is out (spent window or a cap the
/// gateway would enforce), `Ok(Some(standing))` when it has measured room, and
/// `Ok(None)` when it is eligible but unmeasurable — usage unreadable, or read
/// with no percent window to compare. The three cases are what both strategies
/// need, so they share one evaluation and can never disagree about who is out.
fn availability(
    candidate: &Candidate,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<Standing>, Blocked> {
    if !candidate.usage_known {
        return Ok(None);
    }
    let applicable: Vec<&UsageWindow> =
        candidate.windows.iter().filter(|w| window_applies(w, model)).collect();

    // A spent window is disqualifying on its own; a configured cap
    // disqualifies through the same evaluator the gateway uses, so a pick and
    // the gateway can never disagree about who is available.
    if let Some(window) = spent_window(&applicable) {
        return Err(Blocked {
            name: candidate.name.clone(),
            reason: format!(
                "{} at {}%{}",
                window.label,
                window.utilization.round() as i64,
                resets_phrase(window, now)
            ),
        });
    }
    let owned: Vec<UsageWindow> = applicable.iter().map(|w| (*w).clone()).collect();
    if let Decision::Block { reason, .. } =
        evaluate_soft_limit(&owned, &candidate.limits, model, now)
    {
        return Err(Blocked { name: candidate.name.clone(), reason });
    }
    let penalty = pace_penalty(candidate, &applicable, now);
    let load = f64::from(candidate.in_flight).mul_add(RESERVATION, 1.0);
    Ok(tightest_window(&applicable, now).map(|(rate, window)| Standing {
        score: rate / penalty / load,
        headroom_pct: 100.0 - window.utilization,
        resets_at: window.resets_at,
    }))
}

/// Divisor discounting the score of an account that is burning faster than its
/// windows' linear budget, so a new session prefers a calmer sibling with the
/// same room. `1.0` (no discount) unless the account opted into pace limits and
/// some applicable window is actually over pace.
fn pace_penalty(candidate: &Candidate, applicable: &[&UsageWindow], now: DateTime<Utc>) -> f64 {
    if !candidate.limits.limits.values().any(|l| l.pace_cap.is_some()) {
        return 1.0;
    }
    applicable
        .iter()
        .filter(|w| is_percent_window(w))
        .filter_map(|w| crate::pace::for_window(now, w, None))
        .map(|p| p.ratio)
        .fold(1.0_f64, f64::max)
}

/// Walk `candidates` in the order given and take the first one with room — the
/// `ordered` pool strategy.
///
/// A ladder, not a balancer: it exists because "spend the cheap account first,
/// only spill to the expensive one when it is dry" is a real policy that no
/// amount of headroom ranking expresses. Position beats margin entirely; a
/// member at 95% used still wins over an untouched one below it.
///
/// Unmeasurable members are eligible in place, on the same fail-open reasoning
/// as [`pick_account`]: an unreadable usage endpoint must degrade the choice,
/// never skip a rung the user deliberately put first.
pub fn pick_in_order(candidates: &[Candidate], model: Option<&str>, now: DateTime<Utc>) -> Pick {
    if candidates.is_empty() {
        return Pick::None;
    }
    let mut blocked: Vec<Blocked> = Vec::new();
    for candidate in candidates {
        match availability(candidate, model, now) {
            Err(reason) => blocked.push(reason),
            Ok(standing) => {
                return Pick::Chosen {
                    name: candidate.name.clone(),
                    headroom_pct: standing.map(|s| s.headroom_pct),
                    score: standing.map(|s| s.score),
                    resets_at: standing.and_then(|s| s.resets_at),
                };
            }
        }
    }
    Pick::Exhausted(blocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soft_limit::{KEY_SESSION, KEY_WEEKLY_ALL, SoftLimit};
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 31, 12, 0, 0).unwrap()
    }

    fn window(key: &str, label: &str, utilization: f64, resets_in_hours: i64) -> UsageWindow {
        UsageWindow {
            key: key.to_owned(),
            kind: "session".to_owned(),
            label: label.to_owned(),
            utilization,
            amount_usd: None,
            resets_at: Some(now() + chrono::Duration::hours(resets_in_hours)),
            model_id: None,
            model_display_name: None,
        }
    }

    fn candidate(name: &str, windows: Vec<UsageWindow>) -> Candidate {
        Candidate {
            name: name.to_owned(),
            windows,
            limits: SoftLimits::default(),
            usage_known: true,
            in_flight: 0,
        }
    }

    /// A pick's name, or a panic naming what came back instead.
    fn chosen(pick: &Pick) -> &str {
        let Pick::Chosen { name, .. } = pick else { panic!("expected a pick, got {pick:?}") };
        name
    }

    /// A pick's raw margin, for the assertions that pin the logged value.
    fn chosen_headroom(pick: &Pick) -> Option<f64> {
        let Pick::Chosen { headroom_pct, .. } = pick else {
            panic!("expected a pick, got {pick:?}")
        };
        *headroom_pct
    }

    /// Equal room, opposite burn rates: both at 40% of the 5h window, but
    /// `aburner` got there in one hour and `zcalm` over four. Alphabetical
    /// order would elect `aburner`; the pace penalty must not let it.
    #[test]
    fn the_calmer_account_wins_a_tie_on_room() {
        let paced = |name: &str, resets_in_hours: i64| Candidate {
            name: name.to_owned(),
            windows: vec![window(KEY_SESSION, "5h", 40.0, resets_in_hours)],
            limits: SoftLimits {
                limits: std::iter::once((
                    KEY_SESSION.to_owned(),
                    SoftLimit { pace_cap: Some(4.0), ..SoftLimit::default() },
                ))
                .collect(),
            },
            usage_known: true,
            in_flight: 0,
        };
        let candidates = vec![paced("aburner", 4), paced("zcalm", 1)];
        assert_eq!(chosen(&pick_account(&candidates, None, now())), "zcalm");

        // Without a pace cap the calmer account still wins: the same 60% left
        // over one hour is a far faster sustainable rate than over four. Before
        // the score weighed time, this fell through to the name tiebreak.
        let unpaced: Vec<Candidate> = candidates
            .iter()
            .cloned()
            .map(|c| Candidate { limits: SoftLimits::default(), ..c })
            .collect();
        assert_eq!(chosen(&pick_account(&unpaced, None, now())), "zcalm");

        // And the penalty still bites on its own: `aburner` is at twice its
        // linear pace, so its score is halved (60% over 4h = 15%/h, then 7.5).
        let Pick::Chosen { score, .. } = pick_account(&candidates[..1], None, now()) else {
            panic!("expected a pick")
        };
        assert!((score.unwrap() - 7.5).abs() < 1e-9, "{score:?}");
    }

    #[test]
    fn ordered_walks_the_ladder_and_ignores_margin() {
        // Second rung has far more room; the ladder still elects the first.
        let candidates = vec![
            candidate("cheap", vec![window(KEY_SESSION, "5h", 80.0, 3)]),
            candidate("spare", vec![window(KEY_SESSION, "5h", 1.0, 3)]),
        ];
        let Pick::Chosen { name, headroom_pct, .. } = pick_in_order(&candidates, None, now())
        else {
            panic!("expected a pick");
        };
        assert_eq!(name, "cheap");
        assert_eq!(headroom_pct, Some(20.0));
        // Headroom ranking, on the very same list, would have gone the other
        // way — which is exactly why the two strategies both exist.
        assert!(matches!(
            pick_account(&candidates, None, now()),
            Pick::Chosen { ref name, .. } if name == "spare"
        ));
    }

    #[test]
    fn ordered_skips_a_spent_rung_and_keeps_walking() {
        let candidates = vec![
            candidate("first", vec![window(KEY_SESSION, "5h", 100.0, 2)]),
            candidate("second", vec![window(KEY_WEEKLY_ALL, "weekly", 100.0, 40)]),
            candidate("third", vec![window(KEY_SESSION, "5h", 42.0, 3)]),
        ];
        let Pick::Chosen { name, .. } = pick_in_order(&candidates, None, now()) else {
            panic!("expected a pick");
        };
        assert_eq!(name, "third");
    }

    #[test]
    fn ordered_reports_every_blocked_rung_when_all_are_out() {
        let candidates = vec![
            candidate("first", vec![window(KEY_SESSION, "5h", 100.0, 2)]),
            candidate("second", vec![window(KEY_SESSION, "5h", 100.0, 5)]),
        ];
        let Pick::Exhausted(blocked) = pick_in_order(&candidates, None, now()) else {
            panic!("expected exhaustion");
        };
        assert_eq!(blocked.len(), 2);
        assert!(blocked.iter().any(|b| b.name == "first"));
        assert!(blocked.iter().any(|b| b.name == "second"));
    }

    #[test]
    fn ordered_keeps_an_unreadable_rung_in_place() {
        // Fail open, and in position: an account whose usage endpoint is down
        // must not silently demote the rung its user put first.
        let mut unreadable = candidate("first", vec![]);
        unreadable.usage_known = false;
        let candidates =
            vec![unreadable, candidate("second", vec![window(KEY_SESSION, "5h", 1.0, 3)])];
        let Pick::Chosen { name, headroom_pct, .. } = pick_in_order(&candidates, None, now())
        else {
            panic!("expected a pick");
        };
        assert_eq!(name, "first");
        assert_eq!(headroom_pct, None);
    }

    #[test]
    fn ordered_on_an_empty_pool_is_none() {
        assert_eq!(pick_in_order(&[], Some("opus"), now()), Pick::None);
    }

    /// The accounts page as screenshotted when this was specified: Claudo idle
    /// on its 5h window but weekly-spent, Patrigeon busier on 5h but with room
    /// everywhere. Ranking on 5h alone would elect Claudo, which cannot serve a
    /// single request.
    fn real_world() -> Vec<Candidate> {
        vec![
            candidate(
                "Claudo",
                vec![
                    window(KEY_SESSION, "5h", 0.0, 1),
                    window(KEY_WEEKLY_ALL, "Weekly (all models)", 100.0, 16),
                    window("weekly_model:fable", "Weekly Fable", 85.0, 16),
                ],
            ),
            candidate(
                "Patrigeon",
                vec![
                    window(KEY_SESSION, "5h", 15.0, 3),
                    window(KEY_WEEKLY_ALL, "Weekly (all models)", 48.0, 96),
                    window("weekly_model:fable", "Weekly Fable", 4.0, 96),
                ],
            ),
        ]
    }

    #[test]
    fn picks_the_widest_narrowest_margin_not_the_idlest_window() {
        let pick = pick_account(&real_world(), Some("opus"), now());
        // The weekly window decides: 52% over 96h is a slower rate than 85% of
        // a 5h window resetting in 3h.
        assert_eq!(
            pick,
            Pick::Chosen {
                name: "Patrigeon".to_owned(),
                headroom_pct: Some(52.0),
                score: Some(52.0 / 96.0),
                resets_at: Some(now() + chrono::Duration::hours(96)),
            }
        );
    }

    #[test]
    fn a_scoped_window_only_counts_for_its_own_model() {
        // One account, plenty of general room, but its Fable budget is spent.
        let candidates = vec![candidate(
            "Solo",
            vec![
                window(KEY_SESSION, "5h", 5.0, 1),
                window(KEY_WEEKLY_ALL, "Weekly (all models)", 10.0, 16),
                window("weekly_model:claude-fable-5", "Weekly Fable", 100.0, 16),
            ],
        )];

        // An Opus session does not care about the Fable window.
        let pick = pick_account(&candidates, Some("opus"), now());
        assert_eq!(chosen(&pick), "Solo");
        assert_eq!(chosen_headroom(&pick), Some(90.0));

        // A Fable session does, and there is nothing left for it. The alias
        // `fable` must match the window's `claude-fable-5` key.
        let Pick::Exhausted(blocked) = pick_account(&candidates, Some("fable"), now()) else {
            panic!("expected exhaustion on the Fable window")
        };
        assert!(blocked[0].reason.contains("Weekly Fable at 100%"), "{:?}", blocked[0]);
    }

    #[test]
    fn an_unknown_model_keeps_every_window() {
        let Pick::Chosen { name, headroom_pct, .. } = pick_account(&real_world(), None, now())
        else {
            panic!("expected a pick")
        };
        assert_eq!(name, "Patrigeon");
        assert_eq!(headroom_pct, Some(52.0));
    }

    #[test]
    fn a_spent_window_disqualifies_even_with_no_cap_configured() {
        let only_claudo = vec![real_world().remove(0)];
        let Pick::Exhausted(blocked) = pick_account(&only_claudo, Some("opus"), now()) else {
            panic!("expected exhaustion")
        };
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].name, "Claudo");
        assert!(blocked[0].reason.contains("Weekly (all models) at 100%"), "{:?}", blocked[0]);
        assert!(blocked[0].reason.contains("resets in 16h"), "{:?}", blocked[0]);
    }

    #[test]
    fn a_configured_cap_disqualifies_before_the_window_is_spent() {
        let mut candidates = real_world();
        candidates[1].limits.limits.insert(
            KEY_WEEKLY_ALL.to_owned(),
            SoftLimit { cap_pct: Some(40), ..SoftLimit::default() },
        );
        // Patrigeon is at 48% against a 40% cap, Claudo is weekly-spent: nobody left.
        let Pick::Exhausted(blocked) = pick_account(&candidates, Some("opus"), now()) else {
            panic!("expected exhaustion")
        };
        assert_eq!(blocked.len(), 2);
        assert_eq!(blocked[0].name, "Claudo");
        assert!(blocked[1].reason.contains("cap 40%"), "{:?}", blocked[1]);
    }

    #[test]
    fn unreadable_usage_ranks_behind_a_measured_account_but_stays_eligible() {
        let mut candidates = real_world();
        candidates.push(Candidate {
            name: "Alphabetically-first".to_owned(),
            windows: vec![],
            limits: SoftLimits::default(),
            usage_known: false,
            in_flight: 0,
        });
        // Patrigeon is measured and has room, so it wins despite the unknown
        // sorting first by name.
        let Pick::Chosen { name, .. } = pick_account(&candidates, Some("opus"), now()) else {
            panic!("expected a pick")
        };
        assert_eq!(name, "Patrigeon");
    }

    #[test]
    fn unreadable_usage_never_reads_as_exhausted() {
        let candidates = vec![
            Candidate {
                name: "Zeta".to_owned(),
                windows: vec![],
                limits: SoftLimits::default(),
                usage_known: false,
                in_flight: 0,
            },
            Candidate {
                name: "Alpha".to_owned(),
                windows: vec![],
                limits: SoftLimits::default(),
                usage_known: false,
                in_flight: 0,
            },
        ];
        // No data anywhere: deterministic fallback, never an error.
        assert_eq!(
            pick_account(&candidates, Some("opus"), now()),
            Pick::Chosen {
                name: "Alpha".to_owned(),
                headroom_pct: None,
                score: None,
                resets_at: None
            }
        );
    }

    #[test]
    fn a_spent_account_alongside_an_unreadable_one_still_launches() {
        let mut candidates = vec![real_world().remove(0)];
        candidates.push(Candidate {
            name: "Unknown".to_owned(),
            windows: vec![],
            limits: SoftLimits::default(),
            usage_known: false,
            in_flight: 0,
        });
        assert_eq!(
            pick_account(&candidates, Some("opus"), now()),
            Pick::Chosen {
                name: "Unknown".to_owned(),
                headroom_pct: None,
                score: None,
                resets_at: None,
            }
        );
    }

    #[test]
    fn ties_break_on_name_so_the_choice_is_reproducible() {
        let candidates = vec![
            candidate("Zeta", vec![window(KEY_SESSION, "5h", 10.0, 1)]),
            candidate("Alpha", vec![window(KEY_SESSION, "5h", 10.0, 1)]),
        ];
        let pick = pick_account(&candidates, Some("opus"), now());
        assert_eq!(chosen(&pick), "Alpha");
        assert_eq!(chosen_headroom(&pick), Some(90.0));
    }

    #[test]
    fn no_candidates_is_not_exhaustion() {
        assert_eq!(pick_account(&[], Some("opus"), now()), Pick::None);
    }

    fn weekly(name: &str, utilization: f64, resets_in_hours: i64) -> Candidate {
        candidate(name, vec![window(KEY_WEEKLY_ALL, "weekly", utilization, resets_in_hours)])
    }

    #[test]
    fn same_margin_the_closer_reset_wins() {
        // Same 35% left; spending it over 2h is room, over 18h it is not.
        let candidates = vec![weekly("Afar", 65.0, 18), weekly("Bsoon", 65.0, 2)];
        let pick = pick_account(&candidates, None, now());
        assert_eq!(chosen(&pick), "Bsoon");
        let Pick::Chosen { score, resets_at, .. } = pick else { unreachable!() };
        assert_eq!(score, Some(35.0 / 2.0));
        assert_eq!(resets_at, Some(now() + chrono::Duration::hours(2)));
    }

    #[test]
    fn a_smaller_margin_about_to_reset_beats_a_larger_one_far_from_it() {
        // 20% left for the next hour against 35% left for the next 18 hours.
        let candidates = vec![weekly("Awide", 65.0, 18), weekly("Zimminent", 80.0, 1)];
        let pick = pick_account(&candidates, None, now());
        assert_eq!(chosen(&pick), "Zimminent");
        // What is logged stays the raw margin of the window that decided.
        assert_eq!(chosen_headroom(&pick), Some(20.0));
    }

    #[test]
    fn a_reset_already_due_is_floored_not_divided_by_zero() {
        let candidates = vec![weekly("Adue", 90.0, 0)];
        let Pick::Chosen { score, .. } = pick_account(&candidates, None, now()) else {
            panic!("expected a pick")
        };
        assert_eq!(score, Some(10.0 / MIN_HOURS_TO_RESET));
    }

    /// The night of 21 to 22 September 2026: a `headroom` pool of three
    /// Anthropic accounts, A with 35% of its weekly left and a reset 18h away,
    /// B with 95% left and a reset a week away, C with 23% left and 3 days to
    /// go. The cockpit dispatched 16 sessions in three hours; the usage read
    /// between two spawns never moved, so all 16 landed on A, which hit its
    /// weekly wall with every one of them at once. Counting the sessions
    /// already in flight must walk the same wave across the whole pool.
    #[test]
    fn a_wave_of_sixteen_spawns_spreads_across_the_pool() {
        let mut pool =
            vec![weekly("A", 65.0, 18), weekly("B", 5.0, 7 * 24), weekly("C", 77.0, 3 * 24)];
        let mut elected = std::collections::BTreeMap::<String, u32>::new();
        for _ in 0..16 {
            let name = chosen(&pick_account(&pool, Some("opus"), now())).to_owned();
            pool.iter_mut().find(|c| c.name == name).unwrap().in_flight += 1;
            *elected.entry(name).or_default() += 1;
        }
        assert_eq!(elected.len(), 3, "every account should take part: {elected:?}");
        // A still takes the most: its 35% over 18h is by far the fastest
        // sustainable rate, just not sixteen sessions' worth.
        assert!(elected["A"] > elected["B"] && elected["B"] > elected["C"], "{elected:?}");
        assert!(elected["A"] < 16);

        // Without the in-flight count, the incident reproduces exactly.
        let fresh =
            vec![weekly("A", 65.0, 18), weekly("B", 5.0, 7 * 24), weekly("C", 77.0, 3 * 24)];
        for _ in 0..16 {
            assert_eq!(chosen(&pick_account(&fresh, Some("opus"), now())), "A");
        }
    }

    #[test]
    fn a_window_without_a_reset_keeps_its_raw_margin() {
        let mut open = weekly("Open", 70.0, 0);
        open.windows[0].resets_at = None;
        // 30% with no reset scores 30; 40% over 10h scores 4.
        let candidates = vec![open, weekly("Timed", 60.0, 10)];
        let Pick::Chosen { name, score, resets_at, headroom_pct } =
            pick_account(&candidates, None, now())
        else {
            panic!("expected a pick")
        };
        assert_eq!(name, "Open");
        assert_eq!(score, Some(30.0));
        assert_eq!(headroom_pct, Some(30.0));
        assert_eq!(resets_at, None);
    }

    #[test]
    fn unreadable_accounts_spread_by_load_and_stay_behind_measured_ones() {
        let unreadable = |name: &str, in_flight: u32| Candidate {
            name: name.to_owned(),
            windows: vec![],
            limits: SoftLimits::default(),
            usage_known: false,
            in_flight,
        };
        // No data anywhere: the less loaded unknown goes first, then the name.
        let candidates = vec![unreadable("Alpha", 2), unreadable("Beta", 0)];
        assert_eq!(chosen(&pick_account(&candidates, None, now())), "Beta");

        // A measured account with room, however loaded, still outranks them.
        let mut busy = weekly("Measured", 90.0, 100);
        busy.in_flight = 50;
        let candidates = vec![unreadable("Alpha", 0), busy];
        assert_eq!(chosen(&pick_account(&candidates, None, now())), "Measured");
    }

    #[test]
    fn a_loaded_pool_is_still_exhausted_only_when_every_member_is_out() {
        let mut spent = weekly("A", 100.0, 18);
        spent.in_flight = 16;
        let mut also_spent = weekly("B", 100.0, 40);
        also_spent.in_flight = 1;
        let Pick::Exhausted(blocked) = pick_account(&[spent, also_spent], None, now()) else {
            panic!("expected exhaustion")
        };
        assert_eq!(blocked.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), vec!["A", "B"]);

        // One member with room, even heavily loaded, keeps the pool open.
        let mut roomy = weekly("C", 99.0, 150);
        roomy.in_flight = 40;
        let candidates = vec![weekly("A", 100.0, 18), roomy];
        assert_eq!(chosen(&pick_account(&candidates, None, now())), "C");
    }

    #[test]
    fn ordered_ignores_load_and_reset_times() {
        let mut first = weekly("first", 90.0, 150);
        first.in_flight = 30;
        let candidates = vec![first, weekly("second", 0.0, 1)];
        assert_eq!(chosen(&pick_in_order(&candidates, None, now())), "first");
    }
}
