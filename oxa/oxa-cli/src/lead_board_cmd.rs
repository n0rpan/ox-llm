//! `oxa lead board`: the lead kanban from a terminal. Every verb is one typed
//! `oxa_work::kanban::Op` applied by the store, so the terminal and the web board are two front
//! ends over the same operations. The actor is taken from the environment, never from the words
//! or from JSON: a person's terminal is the owner, a lead session is the lead, and a worker
//! (`OXA_WORKER_DEPTH` with `OXA_BOARD_ITEM`) may only note or stick its own card.
//!
//! Nothing here starts a worker, samples GitHub or reads a setting. `--json` prints the board,
//! an item, a lane or the applied operation and its sequence number.
use oxa_config::{Dirs, Env};
use oxa_proto::{codes, time::now_ms, OxaError};
use oxa_work::kanban::{
    Actor, Cause, Column, Dispatch, EditFields, Goal, HostPref, Item, Kanban, Measure, MeasureKind,
    MeasureResult, Need, NewChild, Op, Store,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The dispatcher marks the card it handed to a worker with this item id.
pub(crate) const ENV_BOARD_ITEM: &str = "OXA_BOARD_ITEM";

/// A bounded JSON input: the planning chunk or the split children.
const MAX_JSON_INPUT: usize = 256 * 1024;
/// Every flag that takes a value, so a typo is a usage error and never a silent no-op.
const VALUE_FLAGS: &[&str] = &[
    "--repo",
    "--column",
    "--title",
    "--issue",
    "--done-when",
    "--kind",
    "--host",
    "--only",
    "--needs",
    "--model",
    "--before",
    "--from-json",
    "--cause",
    "--detail",
    "--text",
    "--pr",
    "--measure",
    "--target",
    "--evidence",
    "--seq",
    "--pause",
    "--resume",
];
const BOOLEAN_FLAGS: &[&str] = &[
    "--json",
    "--ready",
    "--top",
    "--passed",
    "--failed",
    "--clear",
    "--no-issue",
    "--no-model",
    "--paused",
];

pub fn run_lead_board(
    words: &[String],
    dirs: &Dirs,
    cwd: &Path,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let (verb, args) = Args::parse(words)?;
    let repo = repo(cwd, &args)?;
    let store = open(dirs, &repo)?;
    run_verb(&verb, &args, &repo, &store, env, out)
}

fn run_verb(
    verb: &str,
    args: &Args,
    repo: &str,
    store: &Store,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    match verb {
        "board" => read_board(args, store, out),
        "list" => list(args, store, out),
        "show" => show(args, store, out),
        "add" => add(args, store, env, out),
        "edit" => edit(args, store, env, out),
        "move" | "reorder" => move_card(verb, args, store, env, out),
        "split" => split(args, store, env, out),
        "stuck" => stuck(args, store, env, out),
        "unstick" => unstick(args, store, env, out),
        "note" => note(args, store, env, out),
        "rm" | "remove" => remove(args, store, env, out),
        "undo" => undo(args, store, env, out),
        "link" => link(args, store, env, out),
        "capacity" => capacity(args, store, env, out),
        "goal" => goal(args, store, env, out),
        "check" => check(args, store, env, out),
        "dispatch" => dispatch(args, store, env, out),
        other => Err(usage(&format!(
            "unknown board verb {other:?}; the verbs are listed by `oxa help lead board`"
        ))),
    }
    .map_err(|e| e.with_context(repo))
}

// --- read verbs -------------------------------------------------------------------------

/// `oxa lead board`: the whole board — lanes with counts, capacity, goal and the stuck list.
/// `--json` is the typed `Kanban` itself, so a reader needs no second schema.
fn read_board(args: &Args, store: &Store, out: &mut dyn Write) -> Result<i32, OxaError> {
    let board = store.read().map_err(refused)?;
    if args.json() {
        return json_out(out, &serde_json::to_value(&board).map_err(encoding)?);
    }
    emit(out, &board_text(&board))
}

/// `oxa lead board list [--column C] [--json]`: the cards in pull order, or one lane of them.
/// `--json` is the array of typed items, and an item printed here can be added again.
fn list(args: &Args, store: &Store, out: &mut dyn Write) -> Result<i32, OxaError> {
    let board = store.read().map_err(refused)?;
    let column = args.one("--column").map(parse_column).transpose()?;
    let lanes: Vec<&Item> = match column {
        Some(column) => board.lane(column),
        None => COLUMNS.iter().flat_map(|column| board.lane(*column)).collect(),
    };
    if args.json() {
        let items: Vec<Value> = lanes
            .iter()
            .map(|item| serde_json::to_value(item).map_err(encoding))
            .collect::<Result<_, _>>()?;
        return json_out(out, &Value::Array(items));
    }
    let mut text = String::new();
    for item in lanes {
        let stuck = item
            .stuck
            .as_ref()
            .map(|cause| format!(" [{}]", cause_name(&cause.cause)))
            .unwrap_or_default();
        text.push_str(&format!(
            "K{}  {:9} {:12} {}{}\n",
            item.id,
            column_name(item.column),
            item.kind,
            item.title,
            stuck
        ));
    }
    if text.is_empty() {
        text.push_str("No cards.\n");
    }
    emit(out, &text)
}

fn show(args: &Args, store: &Store, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let board = store.read().map_err(refused)?;
    let item = board
        .item(id)
        .ok_or_else(|| invalid(format!("kanban item K{id} not found")))?;
    if args.json() {
        return json_out(out, &serde_json::to_value(item).map_err(encoding)?);
    }
    emit(out, &item_text(item))
}

fn board_text(board: &Kanban) -> String {
    let mut text = String::new();
    if board.items.is_empty() && board.goal.is_none() {
        text.push_str("No board yet. Add the first item: oxa lead board add --title T --done-when S\n");
    }
    if let Some(goal) = &board.goal {
        text.push_str(&format!("Goal: {}\n", goal.text));
        for measure in &goal.measures {
            text.push_str(&format!(
                "  {} {} {}\n",
                measure.id,
                measure_name(&measure.kind),
                measure_state(&measure.kind)
            ));
        }
        if let Some(target) = goal.target_per_hour {
            text.push_str(&format!("  target {target}/h\n"));
        }
    }
    text.push_str(&format!("Dispatch: {}\n", dispatch_name(board.dispatch)));
    if board.capacity.is_empty() {
        text.push_str("Capacity: not set\n");
    } else {
        let used: BTreeMap<&str, usize> = board
            .items
            .iter()
            .filter(|i| i.column == Column::Running)
            .filter_map(|i| i.run.as_ref().map(|run| run.host.as_str()))
            .fold(BTreeMap::new(), |mut map, host| {
                *map.entry(host).or_default() += 1;
                map
            });
        let line: Vec<String> = board
            .capacity
            .iter()
            .map(|(host, capacity)| {
                let running = used.get(host.as_str()).copied().unwrap_or(0);
                let paused = if capacity.paused { " (paused)" } else { "" };
                format!("{host} {running}/{}{paused}", capacity.slots)
            })
            .collect();
        text.push_str(&format!("Capacity: {}\n", line.join(" · ")));
    }
    for column in COLUMNS {
        let lane = board.lane(column);
        let shown: Vec<&str> = lane.iter().take(3).map(|i| i.title.as_str()).collect();
        let more = lane.len().saturating_sub(shown.len());
        let mut line = format!("{:9} {}", column_name(column), lane.len());
        if !shown.is_empty() {
            line.push_str(&format!(": {}", shown.join(" · ")));
        }
        if more > 0 {
            line.push_str(&format!(" (+{more} more)"));
        }
        text.push_str(&line);
        text.push('\n');
    }
    let stuck: Vec<&Item> = board.items.iter().filter(|i| i.stuck.is_some()).collect();
    text.push_str(&format!("Stuck {}\n", stuck.len()));
    for item in stuck {
        if let Some(cause) = &item.stuck {
            text.push_str(&format!(
                "  K{} {}: {}\n",
                item.id,
                cause_name(&cause.cause),
                cause.detail
            ));
        }
    }
    text
}

fn item_text(item: &Item) -> String {
    let mut text = format!("K{}  {}  [{}]\n", item.id, item.title, column_name(item.column));
    if let Some(issue) = item.issue {
        text.push_str(&format!("  issue #{issue}\n"));
    }
    text.push_str(&format!("  done-when: {}\n", item.done_when));
    text.push_str(&format!(
        "  kind {}  host {}  rev {}\n",
        item.kind,
        host_name(&item.host),
        item.rev
    ));
    if !item.needs.is_empty() {
        let needs: Vec<&str> = item.needs.iter().map(need_name).collect();
        text.push_str(&format!("  needs {}\n", needs.join(",")));
    }
    if let Some(model) = &item.model {
        text.push_str(&format!("  model {model}\n"));
    }
    if let Some(pr) = &item.pr {
        text.push_str(&format!("  pr #{}\n", pr.number));
    }
    if let Some(run) = &item.run {
        text.push_str(&format!("  running {} on {}\n", run.worker, run.host));
    }
    if let Some(stuck) = &item.stuck {
        text.push_str(&format!(
            "  stuck {}: {}\n",
            cause_name(&stuck.cause),
            stuck.detail
        ));
    }
    if let Some(note) = &item.note {
        text.push_str(&format!("  note: {note}\n"));
    }
    text
}

// --- write verbs ------------------------------------------------------------------------

fn add(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let actor = actor(env)?;
    if let Some(source) = args.one("--from-json") {
        let cards: Vec<NewCard> = parse_json(&read_json_input(source)?)?;
        let before = store.read().map_err(refused)?;
        let mut applied = Vec::new();
        for card in cards {
            let op = card.into_op(args)?;
            let result = store.apply(actor.clone(), op).map_err(refused)?;
            applied.push(result.seq);
        }
        let ids = add_ids(&before, store);
        return report_many(out, args, &applied, &ids);
    }
    let card = NewCard {
        title: args.one("--title").map(str::to_owned),
        issue: args.one("--issue").map(parse_number).transpose()?,
        done_when: args.one("--done-when").unwrap_or_default().to_owned(),
        kind: args.one("--kind").map(str::to_owned),
        model: args.one("--model").map(str::to_owned),
        host: Some(host_pref(args)?),
        needs: needs(args)?,
        column: None,
    };
    let before = store.read().map_err(refused)?;
    let result = store.apply(actor, card.into_op(args)?).map_err(refused)?;
    let id = added_id(&before, store);
    report_one(out, args, result.seq, id)
}

fn edit(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let before = store.read().map_err(refused)?;
    let item = before
        .item(id)
        .ok_or_else(|| invalid(format!("kanban item K{id} not found")))?;
    let revision = match args.one("--seq") {
        Some(value) => parse_number(value)?,
        None => item.rev,
    };
    let model = if args.flag("--no-model") {
        Some(None)
    } else {
        args.one("--model").map(|m| Some(m.to_owned()))
    };
    let fields = EditFields {
        title: args.one("--title").map(str::to_owned),
        done_when: args.one("--done-when").map(str::to_owned),
        kind: args.one("--kind").map(str::to_owned),
        model,
        host: (args.one("--host").is_some() || args.one("--only").is_some())
            .then(|| host_pref(args))
            .transpose()?,
        needs: args.one("--needs").map(|_| needs(args)).transpose()?,
        issue: if args.flag("--no-issue") {
            Some(None)
        } else {
            args.one("--issue")
                .map(|v| parse_number(v).map(Some))
                .transpose()?
        },
    };
    let actor = actor(env)?;
    let result = store
        .apply(
            actor,
            Op::Edit {
                id,
                rev: revision,
                fields,
            },
        )
        .map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

/// `move K41 COLUMN [--before K12 | --top]` and `reorder K41 [--before K12 | --top]`. Moves and
/// reorders are relative, so a second writer never overwrites the whole board.
fn move_card(
    verb: &str,
    args: &Args,
    store: &Store,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let before = store.read().map_err(refused)?;
    let item = before
        .item(id)
        .ok_or_else(|| invalid(format!("kanban item K{id} not found")))?;
    let to = if verb == "reorder" {
        item.column
    } else {
        let column = args
            .positional(1)
            .or_else(|| args.one("--column"))
            .ok_or_else(|| usage("move needs a target column"))?;
        parse_column(column)?
    };
    let target = if args.flag("--top") {
        before.lane(to).iter().find(|i| i.id != id).map(|i| i.id)
    } else {
        args.one("--before").map(parse_id).transpose()?
    };
    let op = if verb == "reorder" {
        Op::Reorder { id, before: target }
    } else {
        Op::Move {
            id,
            to,
            before: target,
        }
    };
    let actor = actor(env)?;
    let result = store.apply(actor, op).map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

fn split(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let before = store.read().map_err(refused)?;
    let item = before
        .item(id)
        .ok_or_else(|| invalid(format!("kanban item K{id} not found")))?;
    let revision = item.rev;
    let source = args
        .one("--from-json")
        .ok_or_else(|| usage("split needs --from-json <path|-> with the child cards"))?;
    let children: Vec<NewChild> = parse_json(&read_json_input(source)?)?;
    let actor = actor(env)?;
    let result = store
        .apply(
            actor,
            Op::Split {
                id,
                rev: revision,
                children,
            },
        )
        .map_err(refused)?;
    report_one(out, args, result.seq, None)
}

fn stuck(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let cause = match args.one("--cause") {
        Some(spec) => parse_cause(spec)?,
        None => Cause::Manual,
    };
    let detail = args.one("--detail").unwrap_or_default().to_owned();
    let actor = actor(env)?;
    let result = store
        .apply(actor, Op::SetStuck { id, cause, detail })
        .map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

fn unstick(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let actor = actor(env)?;
    let result = store.apply(actor, Op::Unstick { id }).map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

fn note(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let text = args
        .one("--text")
        .ok_or_else(|| usage("note needs --text"))?
        .to_owned();
    let actor = actor(env)?;
    let result = store.apply(actor, Op::Note { id, text }).map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

fn remove(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let actor = actor(env)?;
    let result = store.apply(actor, Op::Remove { id }).map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

fn undo(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let board = store.read().map_err(refused)?;
    let seq = match args.one("--seq") {
        Some(value) => parse_number(value)?,
        None => board.revision,
    };
    let actor = actor(env)?;
    let result = store.apply(actor, Op::Undo { seq }).map_err(refused)?;
    report_one(out, args, result.seq, None)
}

fn link(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args.id(0)?;
    let number = args
        .one("--pr")
        .ok_or_else(|| usage("link needs --pr N"))
        .and_then(parse_number)?;
    let actor = actor(env)?;
    let result = store.apply(actor, Op::LinkPr { id, number }).map_err(refused)?;
    report_one(out, args, result.seq, Some(id))
}

fn capacity(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let board = store.read().map_err(refused)?;
    let (host, slots, paused) = if let Some(host) = args.one("--pause").or_else(|| args.one("--resume")) {
        let paused = args.one("--pause").is_some();
        let prior = board
            .capacity
            .get(host)
            .copied()
            .ok_or_else(|| invalid(format!("host {host:?} has no capacity yet; set its slots first")))?;
        (host.to_owned(), prior.slots, paused)
    } else if let (Some(host), Some(slots)) = (args.positional(0), args.positional(1)) {
        let slots = parse_number(slots)?;
        (host.to_owned(), slots, args.flag("--paused"))
    } else if args.one("--slots").is_some() {
        let host = args
            .one("--host")
            .ok_or_else(|| usage("--slots needs --host HOST"))?;
        (
            host.to_owned(),
            parse_number(args.one("--slots").unwrap())?,
            false,
        )
    } else if board.capacity.is_empty() {
        return Err(usage(
            "capacity needs a host and its slots: oxa lead board capacity HOST N",
        ));
    } else {
        if args.json() {
            return json_out(out, &serde_json::to_value(&board.capacity).map_err(encoding)?);
        }
        let line: Vec<String> = board
            .capacity
            .iter()
            .map(|(host, c)| {
                let paused = if c.paused { " (paused)" } else { "" };
                format!("{host} {}{paused}", c.slots)
            })
            .collect();
        return emit(out, &format!("{}\n", line.join(" · ")));
    };
    let actor = actor(env)?;
    let result = store
        .apply(
            actor,
            Op::SetCapacity {
                host: host.clone(),
                slots,
                paused,
            },
        )
        .map_err(refused)?;
    report_one(out, args, result.seq, None)
}

fn goal(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let board = store.read().map_err(refused)?;
    let setting = args.flag("--clear")
        || args.one("--text").is_some()
        || args.one("--target").is_some()
        || !args.many("--measure").is_empty();
    if !setting {
        if args.json() {
            return json_out(out, &serde_json::to_value(&board.goal).map_err(encoding)?);
        }
        return match &board.goal {
            Some(goal) => emit(out, &goal_text(goal)),
            None => emit(out, "No goal set.\n"),
        };
    }
    let prior = board.goal.clone();
    let goal = if args.flag("--clear") {
        None
    } else {
        let text = args
            .one("--text")
            .map(str::to_owned)
            .or_else(|| prior.as_ref().map(|g| g.text.clone()))
            .ok_or_else(|| usage("a goal needs --text"))?;
        let measures = if args.many("--measure").is_empty() {
            prior.as_ref().map(|g| g.measures.clone()).unwrap_or_default()
        } else {
            args.many("--measure")
                .iter()
                .enumerate()
                .map(|(index, spec)| measure(&format!("M{}", index + 1), spec))
                .collect::<Result<_, _>>()?
        };
        let target = match args.one("--target") {
            Some(value) => Some(parse_rate(value)?),
            None => prior.as_ref().and_then(|g| g.target_per_hour),
        };
        Some(Goal {
            text,
            measures,
            target_per_hour: target,
            set_at: now_ms(),
            met_at: None,
        })
    };
    let actor = actor(env)?;
    let result = store.apply(actor, Op::SetGoal { goal }).map_err(refused)?;
    report_one(out, args, result.seq, None)
}

fn goal_text(goal: &Goal) -> String {
    let mut text = format!("Goal: {}\n", goal.text);
    for measure in &goal.measures {
        text.push_str(&format!(
            "  {} {} {}\n",
            measure.id,
            measure_name(&measure.kind),
            measure_state(&measure.kind)
        ));
    }
    if let Some(target) = goal.target_per_hour {
        text.push_str(&format!("  target {target}/h\n"));
    }
    text
}

fn check(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let id = args
        .positional(0)
        .ok_or_else(|| usage("check needs a measure id"))?
        .to_owned();
    let evidence = args
        .one("--evidence")
        .ok_or_else(|| usage("check needs --evidence"))?
        .to_owned();
    let passed = !args.flag("--failed");
    let actor = actor(env)?;
    let result = store
        .apply(
            actor,
            Op::CheckMeasure {
                id,
                result: MeasureResult {
                    passed,
                    evidence,
                    at: now_ms(),
                },
            },
        )
        .map_err(refused)?;
    report_one(out, args, result.seq, None)
}

fn dispatch(args: &Args, store: &Store, env: &Env, out: &mut dyn Write) -> Result<i32, OxaError> {
    let state = match args.positional(0) {
        Some("start") => Dispatch::Running,
        Some("pause") => Dispatch::Paused,
        Some(other) => return Err(usage(&format!("dispatch takes start or pause, not {other:?}"))),
        None => {
            let board = store.read().map_err(refused)?;
            return if args.json() {
                json_out(out, &json!({ "state": board.dispatch }))
            } else {
                emit(out, &format!("Dispatch: {}\n", dispatch_name(board.dispatch)))
            };
        }
    };
    let actor = actor(env)?;
    let result = store.apply(actor, Op::SetDispatch { state }).map_err(refused)?;
    report_one(out, args, result.seq, None)
}

// --- actor, repository and store --------------------------------------------------------

/// The actor the trusted front end supplies: a worker with its own card, a lead session, or
/// the owner at a terminal. It is never read from the words or from JSON.
fn actor(env: &Env) -> Result<Actor, OxaError> {
    if env.get(crate::run_kind::ENV_WORKER_DEPTH).is_some() {
        return match env.get(ENV_BOARD_ITEM) {
            Some(value) => Ok(Actor::Worker(parse_id(value)?)),
            None => Err(OxaError::new(
                codes::DENIED,
                "a worker may only note or stick the card the dispatcher gave it (OXA_BOARD_ITEM)",
            )),
        };
    }
    Ok(if crate::dispatch::is_agent(env) {
        Actor::Lead
    } else {
        Actor::Owner
    })
}

fn repo(cwd: &Path, args: &Args) -> Result<String, OxaError> {
    if let Some(repo) = args.one("--repo") {
        return if oxa_work::valid_repository(repo) {
            Ok(repo.to_owned())
        } else {
            Err(usage("--repo must be OWNER/NAME"))
        };
    }
    let mut command = std::process::Command::new("git");
    command.arg("-C").arg(cwd).args(["remote", "get-url", "origin"]);
    let missing = || invalid("no github origin here; pass --repo OWNER/NAME to name the board repository");
    let (code, out) =
        oxa_work::command_runner(command, Some(4096), Duration::from_secs(10)).ok_or_else(missing)?;
    if code != 0 {
        return Err(missing());
    }
    crate::wait_probe::parse_github_remote(out.trim()).ok_or_else(missing)
}

fn board_dir(dirs: &Dirs, repo: &str) -> PathBuf {
    let key = repo.replace('/', "__");
    dirs.agent_dir.join("kanban").join(key)
}

fn open(dirs: &Dirs, repo: &str) -> Result<Store, OxaError> {
    Store::open(&board_dir(dirs, repo), repo).map_err(refused)
}

// --- reported results -------------------------------------------------------------------

fn report_one(out: &mut dyn Write, args: &Args, seq: u64, id: Option<u32>) -> Result<i32, OxaError> {
    if args.json() {
        return json_out(out, &json!({ "seq": seq, "revision": seq, "id": id }));
    }
    match id {
        Some(id) => emit(out, &format!("K{id} applied (seq {seq})\n")),
        None => emit(out, &format!("applied (seq {seq})\n")),
    }
}

fn report_many(out: &mut dyn Write, args: &Args, seqs: &[u64], ids: &[u32]) -> Result<i32, OxaError> {
    if args.json() {
        let applied: Vec<Value> = seqs
            .iter()
            .enumerate()
            .map(|(index, seq)| json!({ "seq": seq, "revision": seq, "id": ids.get(index) }))
            .collect();
        return json_out(out, &json!({ "applied": applied }));
    }
    let ids: Vec<String> = ids.iter().map(|id| format!("K{id}")).collect();
    emit(
        out,
        &format!(
            "{} cards applied: {} (seq {seqs:?})\n",
            seqs.len(),
            ids.join(", ")
        ),
    )
}

/// The ids an add created, oldest first: the items that were not on the board before. When a
/// second writer added a card at the same time the extra id is included; presentation only.
fn add_ids(before: &Kanban, store: &Store) -> Vec<u32> {
    let Some(after) = store.read().ok() else {
        return Vec::new();
    };
    let known: BTreeSet<u32> = before.items.iter().map(|i| i.id).collect();
    let mut ids: Vec<u32> = after
        .items
        .iter()
        .map(|i| i.id)
        .filter(|id| !known.contains(id))
        .collect();
    ids.sort_unstable();
    ids
}

/// The one id an add created, when exactly one card appeared (otherwise `None`).
fn added_id(before: &Kanban, store: &Store) -> Option<u32> {
    match add_ids(before, store).as_slice() {
        [id] => Some(*id),
        _ => None,
    }
}

// --- text builders ----------------------------------------------------------------------

const COLUMNS: [Column; 6] = [
    Column::Backlog,
    Column::Ready,
    Column::Running,
    Column::PrOpen,
    Column::InQueue,
    Column::Merged,
];

fn column_name(column: Column) -> &'static str {
    match column {
        Column::Backlog => "backlog",
        Column::Ready => "ready",
        Column::Running => "running",
        Column::PrOpen => "pr-open",
        Column::InQueue => "in-queue",
        Column::Merged => "merged",
    }
}

fn parse_column(word: &str) -> Result<Column, OxaError> {
    match word.replace('_', "-").as_str() {
        "backlog" => Ok(Column::Backlog),
        "ready" => Ok(Column::Ready),
        "running" => Ok(Column::Running),
        "pr-open" | "pr" => Ok(Column::PrOpen),
        "in-queue" | "queue" => Ok(Column::InQueue),
        "merged" => Ok(Column::Merged),
        other => Err(usage(&format!("unknown column {other:?}"))),
    }
}

fn dispatch_name(state: Dispatch) -> &'static str {
    match state {
        Dispatch::Paused => "paused",
        Dispatch::Running => "running",
        Dispatch::GoalMet => "goal met",
        Dispatch::Starved => "starved",
    }
}

fn host_name(host: &HostPref) -> String {
    match host {
        HostPref::Any => "any".to_owned(),
        HostPref::Prefer(host) => format!("prefer {host}"),
        HostPref::Only(host) => format!("only {host}"),
    }
}

fn need_name(need: &Need) -> &'static str {
    match need {
        Need::Arm64 => "arm64",
        Need::X86 => "x86",
        Need::RamHeavy => "ram-heavy",
    }
}

fn cause_name(cause: &Cause) -> String {
    match cause {
        Cause::WorkerFailed => "worker-failed".into(),
        Cause::WorkerStalled => "worker-stalled".into(),
        Cause::NoPr => "no-pr".into(),
        Cause::CiRed => "ci-red".into(),
        Cause::ReviewBlocked => "review-blocked".into(),
        Cause::Conflict => "conflict".into(),
        Cause::QueueRejected => "queue-rejected".into(),
        Cause::PrClosed => "pr-closed".into(),
        Cause::NeedsOwner(ask) => format!("needs-owner:{ask}"),
        Cause::HostDown => "host-down".into(),
        Cause::NoModel => "no-model".into(),
        Cause::Manual => "manual".into(),
    }
}

fn parse_cause(spec: &str) -> Result<Cause, OxaError> {
    let normalized = spec.replace('-', "_");
    let cause = match normalized.as_str() {
        "worker_failed" => Cause::WorkerFailed,
        "worker_stalled" => Cause::WorkerStalled,
        "no_pr" => Cause::NoPr,
        "ci_red" => Cause::CiRed,
        "review_blocked" => Cause::ReviewBlocked,
        "conflict" => Cause::Conflict,
        "queue_rejected" => Cause::QueueRejected,
        "pr_closed" => Cause::PrClosed,
        "host_down" => Cause::HostDown,
        "no_model" => Cause::NoModel,
        "manual" => Cause::Manual,
        other => match other.strip_prefix("needs_owner:") {
            Some(ask) if !ask.is_empty() => Cause::NeedsOwner(ask.to_owned()),
            _ => return Err(usage(&format!("unknown stuck cause {spec:?}"))),
        },
    };
    Ok(cause)
}

fn measure_name(kind: &MeasureKind) -> &'static str {
    match kind {
        MeasureKind::IssuesClosed { .. } => "issues-closed",
        MeasureKind::ItemsMerged { .. } => "items-merged",
        MeasureKind::MergedCount { .. } => "merged-count",
        MeasureKind::Manual { .. } => "manual",
    }
}

fn measure_state(kind: &MeasureKind) -> &'static str {
    match kind {
        MeasureKind::Manual {
            result: Some(result), ..
        } if result.passed => "passed",
        MeasureKind::Manual { result: Some(_), .. } => "failed",
        MeasureKind::Manual { result: None, .. } => "open",
        // The reconciler evaluates the GitHub measures in a later slice; unknown stays unknown.
        _ => "unknown",
    }
}

fn measure(id: &str, spec: &str) -> Result<Measure, OxaError> {
    let kind = if spec == "merged-all" {
        MeasureKind::ItemsMerged { parent: None }
    } else if let Some(rest) = spec.strip_prefix("merged:") {
        MeasureKind::ItemsMerged {
            parent: Some(parse_id(rest)?),
        }
    } else if let Some(rest) = spec.strip_prefix("issues:") {
        MeasureKind::IssuesClosed {
            numbers: rest
                .split(',')
                .map(|number| parse_number(number.trim()))
                .collect::<Result<_, _>>()?,
            label: None,
            milestone: None,
        }
    } else if let Some(rest) = spec.strip_prefix("manual:") {
        MeasureKind::Manual {
            acceptance: rest.to_owned(),
            result: None,
        }
    } else {
        return Err(usage(&format!(
            "unknown measure {spec:?}; use issues:N,N, merged-all, merged:K41 or manual:TEXT"
        )));
    };
    Ok(Measure {
        id: id.to_owned(),
        kind,
    })
}

// --- JSON card input --------------------------------------------------------------------

/// One card from `add` or `add --from-json`: the fields of an add and nothing else. Unknown
/// fields are ignored, so an item printed by `show --json` can be added again as a new card.
#[derive(Deserialize)]
struct NewCard {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    issue: Option<u64>,
    #[serde(default)]
    done_when: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    host: Option<HostPref>,
    #[serde(default)]
    needs: BTreeSet<Need>,
    #[serde(default)]
    column: Option<Column>,
}
impl NewCard {
    fn into_op(self, args: &Args) -> Result<Op, OxaError> {
        let title = self
            .title
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| usage("a card needs a title"))?;
        let column = match self.column {
            Some(column) => column,
            None if args.flag("--ready") => Column::Ready,
            None => Column::Backlog,
        };
        Ok(Op::Add {
            title,
            issue: self.issue,
            done_when: self.done_when,
            kind: self
                .kind
                .or_else(|| args.one("--kind").map(str::to_owned))
                .unwrap_or_else(|| "build".to_owned()),
            model: self.model,
            host: self.host.unwrap_or(host_pref(args)?),
            needs: if self.needs.is_empty() {
                needs(args)?
            } else {
                self.needs
            },
            column,
        })
    }
}

fn host_pref(args: &Args) -> Result<HostPref, OxaError> {
    match (args.one("--host"), args.one("--only")) {
        (Some(_), Some(_)) => Err(usage("--host and --only are alternatives")),
        (Some(host), None) => Ok(HostPref::Prefer(host.to_owned())),
        (None, Some(host)) => Ok(HostPref::Only(host.to_owned())),
        (None, None) => Ok(HostPref::Any),
    }
}

fn needs(args: &Args) -> Result<BTreeSet<Need>, OxaError> {
    let mut needs = BTreeSet::new();
    for word in args
        .one("--needs")
        .unwrap_or_default()
        .split(',')
        .filter(|w| !w.trim().is_empty())
    {
        needs.insert(match word.trim().replace('_', "-").as_str() {
            "arm64" => Need::Arm64,
            "x86" | "x86-64" => Need::X86,
            "ram-heavy" => Need::RamHeavy,
            other => return Err(usage(&format!("unknown need {other:?}"))),
        });
    }
    Ok(needs)
}

fn read_json_input(source: &str) -> Result<Vec<u8>, OxaError> {
    let mut bytes = Vec::new();
    if source == "-" {
        std::io::stdin()
            .take(MAX_JSON_INPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| OxaError::io("read board JSON", &e))?;
    } else {
        std::fs::File::open(source)
            .map_err(|e| OxaError::io("open board JSON", &e))?
            .take(MAX_JSON_INPUT as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| OxaError::io("read board JSON", &e))?;
    }
    if bytes.len() > MAX_JSON_INPUT {
        return Err(invalid("board JSON is too large"));
    }
    Ok(bytes)
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<Vec<T>, OxaError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| invalid(&format!("board JSON is not valid: {e}")))?;
    let value = match value {
        Value::Array(items) => Value::Array(items),
        single => Value::Array(vec![single]),
    };
    serde_json::from_value(value).map_err(|e| invalid(&format!("board JSON is not a card list: {e}")))
}

// --- words and flags --------------------------------------------------------------------

/// The parsed words: a verb, its non-flag positionals and the flags with their values. A
/// boolean flag is present in `flags` with an empty list.
struct Args {
    positional: Vec<String>,
    flags: BTreeMap<String, Vec<String>>,
    json: bool,
}

impl Args {
    fn parse(words: &[String]) -> Result<(String, Args), OxaError> {
        let Some(first) = words.first() else {
            return Ok(("board".to_owned(), Args::new()));
        };
        if first.starts_with('-') {
            // `oxa lead board --json` shows the board, with JSON output.
            let mut args = Args::new();
            args.absorb(words)?;
            return Ok(("board".into(), args));
        }
        let mut args = Args::new();
        args.absorb(&words[1..])?;
        Ok((first.clone(), args))
    }

    fn new() -> Self {
        Self {
            positional: Vec::new(),
            flags: BTreeMap::new(),
            json: false,
        }
    }

    fn absorb(&mut self, words: &[String]) -> Result<(), OxaError> {
        let mut rest = words.iter();
        while let Some(word) = rest.next() {
            if !word.starts_with("--") {
                self.positional.push(word.clone());
                continue;
            }
            let (name, inline) = match word.split_once('=') {
                Some((name, value)) => (name, Some(value.to_owned())),
                None => (word.as_str(), None),
            };
            if name == "--json" {
                self.json = true;
                continue;
            }
            if BOOLEAN_FLAGS.contains(&name) {
                self.flags.entry(name.to_owned()).or_default();
                if let Some(value) = inline {
                    return Err(usage(&format!("{name} does not take a value ({value})")));
                }
                continue;
            }
            if !VALUE_FLAGS.contains(&name) {
                return Err(usage(&format!("unknown board flag {name}")));
            }
            let value = match inline {
                Some(value) => value,
                None => rest
                    .next()
                    .cloned()
                    .ok_or_else(|| usage(&format!("{name} needs a value")))?,
            };
            self.flags.entry(name.to_owned()).or_default().push(value);
        }
        Ok(())
    }

    fn json(&self) -> bool {
        self.json
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }

    fn one(&self, name: &str) -> Option<&str> {
        self.many(name).last().map(String::as_str)
    }

    fn many(&self, name: &str) -> &[String] {
        self.flags.get(name).map(Vec::as_slice).unwrap_or_default()
    }

    fn positional(&self, index: usize) -> Option<&str> {
        self.positional.get(index).map(String::as_str)
    }

    /// The `K41`/`41` card id at one positional, the required first positional.
    fn id(&self, index: usize) -> Result<u32, OxaError> {
        self.positional(index)
            .ok_or_else(|| usage("this verb needs a card id (K41 or 41)"))
            .and_then(parse_id)
    }
}

fn parse_id(word: &str) -> Result<u32, OxaError> {
    let digits = word.trim().strip_prefix(['K', 'k']).unwrap_or(word.trim());
    parse_number(digits).map_err(|_| usage(&format!("{word:?} is not a card id (K41)")))
}

fn parse_number<T: std::str::FromStr>(word: &str) -> Result<T, OxaError> {
    word.trim()
        .parse()
        .map_err(|_| usage(&format!("{word:?} is not a number")))
}

fn parse_rate(word: &str) -> Result<f32, OxaError> {
    parse_number::<f32>(word).and_then(|rate| {
        if rate.is_finite() && rate >= 0.0 {
            Ok(rate)
        } else {
            Err(usage("the target must be a finite non-negative rate"))
        }
    })
}

// --- output -----------------------------------------------------------------------------

fn emit(out: &mut dyn Write, text: &str) -> Result<i32, OxaError> {
    out.write_all(text.as_bytes())
        .map_err(|e| OxaError::io("write board output", &e))?;
    Ok(0)
}

fn json_out(out: &mut dyn Write, value: &Value) -> Result<i32, OxaError> {
    serde_json::to_writer(&mut *out, value).map_err(encoding)?;
    out.write_all(b"\n")
        .map_err(|e| OxaError::io("write board output", &e))?;
    Ok(0)
}

fn encoding(error: serde_json::Error) -> OxaError {
    OxaError::new(codes::INTERNAL, format!("cannot encode board JSON: {error}"))
}

fn usage(message: &str) -> OxaError {
    OxaError::new(codes::USAGE, message.to_owned())
}

fn invalid(message: impl Into<String>) -> OxaError {
    OxaError::new(codes::INVALID, message.into())
}

/// A store refusal (a stale edit, a missing card, an authority rule) as a refused command.
fn refused(error: oxa_work::kanban::Error) -> OxaError {
    let message = match &error {
        oxa_work::kanban::Error::Stale { current, attempted } => format!(
            "K{} changed since it was opened (rev {}): current {:?}, yours {:?}",
            current.id, current.rev, current.title, attempted.title
        ),
        other => other.to_string(),
    };
    OxaError::new(codes::INVALID, message)
}

trait WithContext {
    fn with_context(self, repo: &str) -> Self;
}
impl WithContext for OxaError {
    fn with_context(self, repo: &str) -> Self {
        if self.code == codes::USAGE {
            self
        } else {
            OxaError::new(self.code, format!("{repo}: {}", self.message))
        }
    }
}
