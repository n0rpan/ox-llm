//! Typed operations applied under the store lock. Moves and reorders are relative, so two
//! writers moving different cards both win; only a text edit carries the item `rev` and a
//! stale one is refused with both safe versions. Every op records an inverse for undo.
use super::model::*;
use crate::board::{Action, RequiredCi};
use crate::standing::Authority;

pub(crate) struct Outcome {
    pub board: Kanban,
    pub inverse: Inverse,
    /// Items leaving the live board for `archive.jsonl` (a remove or a split parent).
    pub archive: Vec<Item>,
}

fn refused<T>(reason: &str) -> Result<T, Error> {
    Err(Error::Refused(reason.into()))
}
fn item<'a>(board: &'a Kanban, id: u32) -> Result<&'a Item, Error> {
    board
        .item(id)
        .ok_or_else(|| Error::Refused(format!("kanban item K{id} not found")))
}
fn replace(board: &mut Kanban, item: Item) {
    if let Some(slot) = board.items.iter_mut().find(|i| i.id == item.id) {
        *slot = item;
    }
}
fn bump(rev: u32) -> Result<u32, Error> {
    rev.checked_add(1)
        .ok_or_else(|| Error::Refused("kanban item rev exhausted".into()))
}

/// Dense ranks within one column, in current order.
fn reassign_lane(board: &mut Kanban, column: Column) {
    let mut order: Vec<(u32, u32)> = board
        .items
        .iter()
        .filter(|i| i.column == column)
        .map(|i| (i.id, i.rank))
        .collect();
    order.sort_by_key(|&(_, rank)| rank);
    for (position, (id, _)) in order.iter().enumerate() {
        if let Some(slot) = board.items.iter_mut().find(|i| i.id == *id) {
            slot.rank = position as u32;
        }
    }
}

/// Insert an item (not yet in the board) into a column at a dense position. A lead may never
/// place a card above an owner-ranked card; the guard checks the cards that would fall below it.
fn place(
    board: &mut Kanban,
    actor: &Actor,
    mut item: Item,
    column: Column,
    index: usize,
) -> Result<(), Error> {
    let len = board.lane(column).len();
    let index = index.min(len);
    if *actor == Actor::Lead {
        let lane = board.lane(column);
        if lane[index..].iter().any(|i| i.rank_by == Authority::Owner) {
            return refused("the lead cannot rank a card above an owner-placed card");
        }
    }
    for slot in board.items.iter_mut().filter(|i| i.column == column) {
        if slot.rank >= index as u32 {
            slot.rank += 1;
        }
    }
    item.column = column;
    item.rank = index as u32;
    board.items.push(item);
    reassign_lane(board, column);
    Ok(())
}

fn authorize_move(actor: &Actor, from: Column, to: Column) -> Result<(), Error> {
    match actor {
        Actor::Owner => Ok(()),
        Actor::Lead => {
            if matches!(from, Column::Backlog | Column::Ready)
                && matches!(to, Column::Backlog | Column::Ready)
            {
                Ok(())
            } else {
                refused("the lead moves cards only between Backlog and Ready")
            }
        }
        Actor::System => {
            if to.system_only() {
                Ok(())
            } else {
                refused("the system moves cards only into the running/PR lanes")
            }
        }
        Actor::Worker(_) => refused("a worker cannot move cards"),
    }
}

fn authorize_remove(actor: &Actor, item: &Item) -> Result<(), Error> {
    match actor {
        Actor::Owner => Ok(()),
        Actor::Lead if item.by != Authority::Owner => Ok(()),
        Actor::Lead => refused("the lead cannot remove an owner-created item"),
        _ => refused("only the owner or lead removes items"),
    }
}

/// The position a `before` target names in a column, excluding the moved item itself.
fn insertion_index(board: &Kanban, to: Column, id: u32, before: Option<u32>) -> Result<usize, Error> {
    let lane: Vec<u32> = board.lane(to).iter().map(|i| i.id).filter(|&x| x != id).collect();
    match before {
        Some(target) if target != id => lane
            .iter()
            .position(|&x| x == target)
            .ok_or_else(|| Error::Refused("the before target is not in that column".into())),
        Some(_) => refused("a card cannot be placed before itself"),
        None => Ok(lane.len()),
    }
}

pub(crate) fn apply(board: &Kanban, actor: &Actor, op: &Op, at: u64) -> Result<Outcome, Error> {
    let mut next = board.clone();
    let mut archive = Vec::new();
    let inverse = match op {
        Op::Add {
            title,
            issue,
            done_when,
            kind,
            model,
            host,
            needs,
            column,
        } => {
            let authority = actor
                .authority()
                .ok_or_else(|| Error::Refused("only the owner or lead adds items".into()))?;
            if *actor == Actor::Lead && column.system_only() {
                return refused("the lead adds items to Backlog or Ready");
            }
            if next.items.len() >= MAX_ITEMS {
                return refused("kanban item capacity exceeded");
            }
            let id = next.next_id;
            next.next_id = id
                .checked_add(1)
                .ok_or_else(|| Error::Refused("kanban id exhausted".into()))?;
            let rank = next.lane(*column).len() as u32;
            let new = Item {
                id,
                rev: 1,
                title: title.clone(),
                issue: *issue,
                done_when: done_when.clone(),
                kind: kind.clone(),
                model: model.clone(),
                host: host.clone(),
                needs: needs.clone(),
                column: *column,
                rank,
                rank_by: authority,
                stuck: None,
                run: None,
                pr: None,
                parent: None,
                note: None,
                by: authority,
                created_at: at,
                updated_at: at,
                column_since: at,
                merged_at: None,
            };
            validate_item(&new).map_err(Error::Refused)?;
            next.items.push(new);
            Inverse::Item {
                id,
                prior: None,
                post_rev: 1,
            }
        }
        Op::Edit { id, rev, fields } => {
            if actor.authority().is_none() {
                return refused("only the owner or lead edits item text");
            }
            if fields.is_empty() {
                return refused("an edit changes at least one field");
            }
            let before = item(&next, *id)?.clone();
            if before.rev != *rev {
                let attempted = fields.applied_to(&before);
                return Err(Error::Stale {
                    current: Box::new(before),
                    attempted: Box::new(attempted),
                });
            }
            let mut after = fields.applied_to(&before);
            after.rev = bump(before.rev)?;
            after.updated_at = at;
            validate_item(&after).map_err(Error::Refused)?;
            let post_rev = after.rev;
            replace(&mut next, after);
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before)),
                post_rev,
            }
        }
        Op::Move { id, to, before } => {
            let before_item = item(&next, *id)?.clone();
            authorize_move(actor, before_item.column, *to)?;
            if *to == Column::Ready && before_item.done_when.trim().is_empty() {
                return refused("a Ready item needs a done-when");
            }
            let index = insertion_index(&next, *to, *id, *before)?;
            let old_column = before_item.column;
            next.items.retain(|i| i.id != *id);
            reassign_lane(&mut next, old_column);
            let mut moved = before_item.clone();
            moved.rev = bump(before_item.rev)?;
            moved.updated_at = at;
            if *to != old_column {
                moved.column_since = at;
            }
            if let Some(authority) = actor.authority() {
                moved.rank_by = authority;
            }
            let post_rev = moved.rev;
            place(&mut next, actor, moved, *to, index)?;
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before_item)),
                post_rev,
            }
        }
        Op::Reorder { id, before } => {
            if actor.authority().is_none() {
                return refused("only the owner or lead reorders cards");
            }
            let before_item = item(&next, *id)?.clone();
            if *actor == Actor::Lead && !matches!(before_item.column, Column::Backlog | Column::Ready) {
                return refused("the lead reorders cards only in Backlog and Ready");
            }
            let column = before_item.column;
            let index = insertion_index(&next, column, *id, *before)?;
            next.items.retain(|i| i.id != *id);
            reassign_lane(&mut next, column);
            let mut moved = before_item.clone();
            moved.rev = bump(before_item.rev)?;
            moved.updated_at = at;
            if let Some(authority) = actor.authority() {
                moved.rank_by = authority;
            }
            let post_rev = moved.rev;
            place(&mut next, actor, moved, column, index)?;
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before_item)),
                post_rev,
            }
        }
        Op::Split { id, rev, children } => {
            let authority = actor
                .authority()
                .ok_or_else(|| Error::Refused("only the owner or lead splits items".into()))?;
            if children.is_empty() || children.len() > MAX_CHILDREN {
                return refused("a split needs between 1 and 16 children");
            }
            let parent = item(&next, *id)?.clone();
            if parent.rev != *rev {
                return refused("the item changed since it was opened; re-read it to split");
            }
            if next.items.len() - 1 + children.len() > MAX_ITEMS {
                return refused("kanban item capacity exceeded");
            }
            let column = parent.column;
            let parent_rank = parent.rank;
            next.items.retain(|i| i.id != *id);
            for slot in next.items.iter_mut().filter(|i| i.column == column) {
                if slot.rank >= parent_rank {
                    slot.rank += children.len() as u32;
                }
            }
            let mut child_ids = Vec::new();
            let mut child_revs = Vec::new();
            for (offset, child) in children.iter().enumerate() {
                let cid = next.next_id;
                next.next_id = cid
                    .checked_add(1)
                    .ok_or_else(|| Error::Refused("kanban id exhausted".into()))?;
                let new = Item {
                    id: cid,
                    rev: 1,
                    title: child.title.clone(),
                    issue: parent.issue,
                    done_when: child.done_when.clone(),
                    kind: parent.kind.clone(),
                    model: parent.model.clone(),
                    host: parent.host.clone(),
                    needs: parent.needs.clone(),
                    column,
                    rank: parent_rank + offset as u32,
                    rank_by: authority,
                    stuck: None,
                    run: None,
                    pr: None,
                    parent: Some(*id),
                    note: None,
                    by: authority,
                    created_at: at,
                    updated_at: at,
                    column_since: at,
                    merged_at: None,
                };
                validate_item(&new).map_err(Error::Refused)?;
                next.items.push(new);
                child_ids.push(cid);
                child_revs.push((cid, 1));
            }
            reassign_lane(&mut next, column);
            archive.push(parent.clone());
            Inverse::Unsplit {
                parent: Box::new(parent),
                children: child_ids,
                child_revs,
            }
        }
        Op::SetStuck { id, cause, detail } => {
            let before = item(&next, *id)?.clone();
            match actor {
                Actor::Owner | Actor::Lead => {}
                Actor::Worker(own) if *own == *id => {}
                Actor::System if cause.automatic() => {}
                Actor::Worker(_) => return refused("a worker sticks only its own item"),
                Actor::System => return refused("the system sets automatic stuck causes, not Manual"),
            }
            text(detail, MAX_DETAIL, false).map_err(Error::Refused)?;
            if let Cause::NeedsOwner(ask) = cause {
                text(ask, 128, true).map_err(Error::Refused)?;
            }
            let mut after = before.clone();
            after.stuck = Some(Stuck {
                cause: cause.clone(),
                detail: detail.clone(),
                since: at,
                by: actor.clone(),
            });
            after.rev = bump(before.rev)?;
            after.updated_at = at;
            let post_rev = after.rev;
            replace(&mut next, after);
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before)),
                post_rev,
            }
        }
        Op::Unstick { id } => {
            if actor.authority().is_none() {
                return refused("only the owner or lead unsticks an item");
            }
            let before = item(&next, *id)?.clone();
            if before.stuck.is_none() {
                return refused("the item is not stuck");
            }
            let mut after = before.clone();
            after.stuck = None;
            after.rev = bump(before.rev)?;
            after.updated_at = at;
            let post_rev = after.rev;
            replace(&mut next, after);
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before)),
                post_rev,
            }
        }
        Op::Remove { id } => {
            let before = item(&next, *id)?.clone();
            authorize_remove(actor, &before)?;
            next.items.retain(|i| i.id != *id);
            reassign_lane(&mut next, before.column);
            archive.push(before.clone());
            Inverse::Restore {
                item: Box::new(before),
            }
        }
        Op::Note { id, text: note } => {
            let before = item(&next, *id)?.clone();
            match actor {
                Actor::Owner => {}
                Actor::Worker(own) if *own == *id => {}
                Actor::Worker(_) => return refused("a worker notes only its own item"),
                _ => return refused("only the owner or the item's worker notes it"),
            }
            text(note, MAX_NOTE, true).map_err(Error::Refused)?;
            let mut after = before.clone();
            after.note = Some(note.clone());
            after.rev = bump(before.rev)?;
            after.updated_at = at;
            let post_rev = after.rev;
            replace(&mut next, after);
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before)),
                post_rev,
            }
        }
        Op::SetCapacity { host, slots, paused } => {
            if *actor != Actor::Owner {
                return refused("only the owner sets capacity");
            }
            if !valid_host(host) || *slots > MAX_SLOTS {
                return refused("capacity needs a valid host and bounded slots");
            }
            let prior = next.capacity.get(host).copied();
            next.capacity.insert(
                host.clone(),
                Capacity {
                    slots: *slots,
                    paused: *paused,
                },
            );
            Inverse::Capacity {
                host: host.clone(),
                prior,
            }
        }
        Op::SetGoal { goal } => {
            if *actor != Actor::Owner {
                return refused("only the owner sets the goal");
            }
            if let Some(goal) = goal {
                validate_goal(goal).map_err(Error::Refused)?;
            }
            let prior = next.goal.clone().map(Box::new);
            next.goal = goal.clone();
            Inverse::Goal { prior }
        }
        Op::CheckMeasure { id, result } => {
            let goal = next
                .goal
                .as_mut()
                .ok_or_else(|| Error::Refused("no goal is set".into()))?;
            let measure = goal
                .measures
                .iter_mut()
                .find(|m| m.id == *id)
                .ok_or_else(|| Error::Refused("measure not found".into()))?;
            let manual = matches!(measure.kind, MeasureKind::Manual { .. });
            if !manual {
                return refused("only a manual measure is ticked; the rest are evaluated from facts");
            }
            match actor {
                Actor::Owner => {}
                Actor::Lead => {}
                _ => return refused("only the owner or lead ticks a manual measure"),
            }
            validate_measure_result(result).map_err(Error::Refused)?;
            let prior = match &mut measure.kind {
                MeasureKind::Manual { result: slot, .. } => slot.replace(result.clone()),
                _ => unreachable!(),
            };
            Inverse::Measure {
                id: id.clone(),
                prior,
            }
        }
        Op::SetDispatch { state } => {
            match actor {
                Actor::Owner => {}
                Actor::System if matches!(state, Dispatch::GoalMet | Dispatch::Starved) => {}
                Actor::System => return refused("the system sets GoalMet or Starved, not on/off"),
                _ => return refused("only the owner turns dispatch on or off"),
            }
            let prior = next.dispatch;
            next.dispatch = *state;
            Inverse::Dispatch { prior }
        }
        Op::LinkPr { id, number } => {
            if actor.authority().is_none() {
                return refused("only the owner or lead links a PR");
            }
            if *number == 0 {
                return refused("a PR number must be positive");
            }
            let before = item(&next, *id)?.clone();
            let mut after = before.clone();
            after.pr = Some(match before.pr.clone() {
                Some(mut pr) => {
                    pr.number = *number;
                    pr
                }
                None => Pr {
                    number: *number,
                    head: String::new(),
                    branch: String::new(),
                    ci: RequiredCi::Pending,
                    next: Action::WaitRequiredCi,
                    queued_at: None,
                    lines: None,
                },
            });
            after.rev = bump(before.rev)?;
            after.updated_at = at;
            let post_rev = after.rev;
            replace(&mut next, after);
            Inverse::Item {
                id: *id,
                prior: Some(Box::new(before)),
                post_rev,
            }
        }
        Op::Undo { .. } => return refused("undo is applied by the store"),
    };
    next.revision = board
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::Refused("kanban revision exhausted".into()))?;
    validate_board(&next).map_err(Error::Refused)?;
    Ok(Outcome {
        board: next,
        inverse,
        archive,
    })
}

/// Restore a prior item snapshot in place of the current one (undo of an item op). The item
/// keeps a monotonic rev; its column and rank return to the prior state, guarded for the lead.
fn restore(board: &mut Kanban, actor: &Actor, prior: Item, at: u64) -> Result<u32, Error> {
    let current = item(board, prior.id)?.clone();
    let mut restored = prior;
    restored.rev = bump(current.rev)?;
    restored.updated_at = at;
    validate_item(&restored).map_err(Error::Refused)?;
    let column = restored.column;
    let index = restored.rank as usize;
    let current_column = current.column;
    board.items.retain(|i| i.id != restored.id);
    reassign_lane(board, current_column);
    let post_rev = restored.rev;
    place(board, actor, restored, column, index)?;
    Ok(post_rev)
}

/// Apply a recorded inverse (undo). The store gates who may undo; this re-checks the effect.
pub(crate) fn apply_inverse(
    board: &Kanban,
    actor: &Actor,
    inverse: &Inverse,
    at: u64,
) -> Result<Outcome, Error> {
    let mut next = board.clone();
    let mut archive = Vec::new();
    match inverse {
        Inverse::Item { id, prior, post_rev } => {
            let current = item(&next, *id)?.clone();
            if current.rev != *post_rev {
                return refused("the item changed since that op; it can no longer be undone");
            }
            match prior {
                None => {
                    authorize_remove(actor, &current)?;
                    next.items.retain(|i| i.id != *id);
                    reassign_lane(&mut next, current.column);
                    archive.push(current);
                }
                Some(prior) => {
                    restore(&mut next, actor, (**prior).clone(), at)?;
                }
            }
        }
        Inverse::Restore { item: prior } => {
            if next.item(prior.id).is_some() {
                return refused("the item id is present again; it cannot be restored");
            }
            authorize_remove(actor, prior)?;
            let column = prior.column;
            let index = prior.rank as usize;
            let mut restored = (**prior).clone();
            restored.updated_at = at;
            place(&mut next, actor, restored, column, index)?;
        }
        Inverse::Unsplit {
            parent,
            children,
            child_revs,
        } => {
            if next.item(parent.id).is_some() {
                return refused("the parent id is present again; the split cannot be undone");
            }
            for (cid, rev) in child_revs {
                let child = item(&next, *cid)?;
                if child.rev != *rev {
                    return refused("a split child changed since; the split can no longer be undone");
                }
            }
            if actor.authority().is_none() {
                return refused("only the owner or lead undoes a split");
            }
            for cid in children {
                if let Some(child) = next.item(*cid) {
                    archive.push(child.clone());
                }
                next.items.retain(|i| i.id != *cid);
            }
            let column = parent.column;
            let index = parent.rank as usize;
            let mut restored = (**parent).clone();
            restored.updated_at = at;
            place(&mut next, actor, restored, column, index)?;
            reassign_lane(&mut next, column);
        }
        Inverse::Capacity { host, prior } => {
            if *actor != Actor::Owner {
                return refused("only the owner undoes a capacity change");
            }
            match prior {
                Some(capacity) => {
                    next.capacity.insert(host.clone(), *capacity);
                }
                None => {
                    next.capacity.remove(host);
                }
            }
        }
        Inverse::Goal { prior } => {
            if *actor != Actor::Owner {
                return refused("only the owner undoes a goal change");
            }
            next.goal = prior.as_deref().cloned();
        }
        Inverse::Measure { id, prior } => {
            let goal = next
                .goal
                .as_mut()
                .ok_or_else(|| Error::Refused("no goal is set".into()))?;
            let measure = goal
                .measures
                .iter_mut()
                .find(|m| m.id == *id)
                .ok_or_else(|| Error::Refused("measure not found".into()))?;
            match &mut measure.kind {
                MeasureKind::Manual { result, .. } => {
                    *result = prior.clone();
                }
                _ => return refused("only a manual measure is ticked"),
            }
            if !matches!(actor, Actor::Owner | Actor::Lead) {
                return refused("only the owner or lead undoes a measure tick");
            }
        }
        Inverse::Dispatch { prior } => {
            match actor {
                Actor::Owner => {}
                Actor::System if matches!(prior, Dispatch::GoalMet | Dispatch::Starved) => {}
                _ => return refused("only the owner undoes an on/off dispatch change"),
            }
            next.dispatch = *prior;
        }
        Inverse::None => return refused("an undo cannot be undone"),
    }
    next.revision = board
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::Refused("kanban revision exhausted".into()))?;
    validate_board(&next).map_err(Error::Refused)?;
    Ok(Outcome {
        board: next,
        inverse: Inverse::None,
        archive,
    })
}
