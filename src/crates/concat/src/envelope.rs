// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer and Concat contributors

//! The volume envelope on the lanes: a clip's volume keys drawn as a line
//! through points on its sound, and the commands its gestures land as.
//!
//! The ride itself is the model's - `KeyProperty::Volume`, which the mixer
//! and the export already follow - so nothing here is new engine; this is
//! the arithmetic of the line and of what a double-click, a drag or a
//! delete on it means, kept out of `Studio` so it can be tested without a
//! window.

use concat_project::Command;
use concat_project::commands::ClipPatch;
use concat_project::model::{Clip, ClipKey, KEY_EPSILON, KeyEase, KeyProperty};

/// The top of the band: twice unity, where the line can be dragged to.
pub const CEILING: f64 = 2.0;

/// How close two points may come in time. A few hairs, not one: closer
/// than `KEY_EPSILON` and a set would land on the neighbour and replace it.
const GAP: f64 = KEY_EPSILON * 3.0;

/// Straight pieces a stretch between two points is drawn and hit-tested
/// in: a linear stretch's, and an eased one's, which bends.
///
/// Fixed counts, never a function of the values, so a drag that changes
/// how steep a stretch is never changes how many pieces it has - the
/// pieces are what the pointer is holding, and a piece that went away
/// mid-drag would drop the gesture.
const LINEAR_STEPS: usize = 8;
const EASED_STEPS: usize = 16;

/// The clip's volume keys, in order.
pub fn keys(clip: &Clip) -> Vec<ClipKey> {
    clip.keys_on(KeyProperty::Volume).copied().collect()
}

/// The ride sampled from the clip's start to its end, as `(at, gain)`:
/// flat before the first key and after the last, and each stretch between
/// two keys in short straight pieces that follow its ease. Empty for a clip
/// whose volume is the one number.
pub fn samples(clip: &Clip) -> Vec<(f64, f64)> {
    let keys = keys(clip);
    let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
        return Vec::new();
    };
    let mut out = vec![(0.0, first.value), (first.at, first.value)];
    for pair in keys.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let steps = if b.ease.is(KeyEase::LINEAR) {
            LINEAR_STEPS
        } else {
            EASED_STEPS
        };
        for step in 1..steps {
            let at = a.at + (b.at - a.at) * step as f64 / steps as f64;
            out.push((at, clip.value_at(KeyProperty::Volume, at)));
        }
        out.push((b.at, b.value));
    }
    out.push((1.0, last.value));
    out
}

/// Samples as SVG path commands in a 1x1 box: across is the clip, down is
/// the band, its top twice unity and its foot silence. Stretched onto the
/// sound by a `fit: fill` Path, the way the waveform is.
pub fn path(samples: &[(f64, f64)]) -> String {
    let mut out = String::new();
    for (index, (at, gain)) in samples.iter().enumerate() {
        let y = 1.0 - gain.clamp(0.0, CEILING) / CEILING;
        let verb = if index == 0 { 'M' } else { 'L' };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{verb} {at:.5} {y:.5}"));
    }
    out
}

/// A double-click on the line at `at`, `0..=1` of the clip: a key there at
/// what the line is already worth, so adding a point changes nothing you
/// can hear until it is dragged. Nothing when a key is already there.
pub fn add(clip: &Clip, at: f64) -> Option<Command> {
    if !at.is_finite() {
        return None;
    }
    let at = at.clamp(0.0, 1.0);
    let keys = keys(clip);
    if keys.iter().any(|key| (key.at - at).abs() < GAP) {
        return None;
    }
    // The ease of the stretch it lands in, so splitting an eased stretch
    // keeps both halves eased; the straight line when it lands outside.
    let ease = keys
        .iter()
        .rev()
        .find(|key| key.at < at)
        .and(keys.iter().find(|key| key.at > at))
        .map_or(KeyEase::LINEAR, |key| key.ease);
    Some(Command::SetClipKey {
        clip_id: clip.id.clone(),
        property: KeyProperty::Volume,
        at,
        value: clip.value_at(KeyProperty::Volume, at),
        ease,
    })
}

/// Key `index` deleted. The last one leaves the clip's constant at the
/// value it had, in the same step, so the line stays where it was drawn
/// instead of jumping to whatever the volume was before it was keyed.
pub fn remove(clip: &Clip, index: usize) -> Option<Command> {
    let keys = keys(clip);
    let key = keys.get(index)?;
    let clear = Command::ClearClipKey {
        clip_id: clip.id.clone(),
        property: KeyProperty::Volume,
        at: key.at,
    };
    if keys.len() > 1 {
        return Some(clear);
    }
    Some(Command::Batch {
        commands: vec![
            clear,
            Command::UpdateClip {
                clip_id: clip.id.clone(),
                patch: ClipPatch {
                    volume: Some(key.value),
                    ..Default::default()
                },
            },
        ],
    })
}

/// Where key `index` of `original` may go when dragged toward `at` and
/// `gain`: no further than a hair short of either neighbour, inside the
/// clip, and between silence and twice unity, with unity snapping so the
/// line finds 100% by feel. `at` is the key's own when it is pinned.
pub fn place(original: &Clip, index: usize, at: f64, gain: f64) -> Option<(f64, f64)> {
    let keys = keys(original);
    keys.get(index)?;
    let low = index
        .checked_sub(1)
        .and_then(|before| keys.get(before))
        .map_or(0.0, |key| key.at + GAP);
    let high = keys.get(index + 1).map_or(1.0, |key| key.at - GAP);
    let at = if at.is_finite() { at } else { keys[index].at };
    let at = at.clamp(low, high.max(low));
    let gain = if gain.is_finite() {
        gain.clamp(0.0, CEILING)
    } else {
        keys[index].value
    };
    let gain = if (gain - 1.0).abs() < 0.04 { 1.0 } else { gain };
    Some((at, gain))
}

/// Writes key `index` of `original`, dragged to `at` and `gain`, onto
/// `clip` - the echo the drag is drawn from. Each move is read against
/// where the drag began, so the key can come back to where it started.
pub fn drag(clip: &mut Clip, original: &Clip, index: usize, at: f64, gain: f64) {
    let Some((at, gain)) = place(original, index, at, gain) else {
        return;
    };
    let key = keys(original)[index];
    clip.keys = original.keys.clone();
    clip.clear_key(KeyProperty::Volume, key.at);
    clip.set_key(KeyProperty::Volume, at, gain, key.ease);
}

/// The commands that take `before`'s volume keys to `after`'s, as one
/// undo step: a key that moved in time is cleared where it was and set
/// where it went, in one batch. None when nothing changed - a press that
/// never travelled.
pub fn commit(clip_id: &str, before: &Clip, after: &Clip) -> Option<Command> {
    let near = |a: f64, b: f64| (a - b).abs() <= KEY_EPSILON;
    let mut commands = Vec::new();
    for key in after.keys_on(KeyProperty::Volume) {
        let unchanged = before.keys_on(KeyProperty::Volume).any(|old| {
            near(old.at, key.at) && (old.value - key.value).abs() < 1e-9 && old.ease == key.ease
        });
        if !unchanged {
            commands.push(Command::SetClipKey {
                clip_id: clip_id.to_owned(),
                property: KeyProperty::Volume,
                at: key.at,
                value: key.value,
                ease: key.ease,
            });
        }
    }
    for old in before.keys_on(KeyProperty::Volume) {
        if !after
            .keys_on(KeyProperty::Volume)
            .any(|key| near(old.at, key.at))
        {
            commands.push(Command::ClearClipKey {
                clip_id: clip_id.to_owned(),
                property: KeyProperty::Volume,
                at: old.at,
            });
        }
    }
    match commands.len() {
        0 => None,
        1 => commands.pop(),
        _ => Some(Command::Batch { commands }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_project::model::ClipKind;

    fn ride(points: &[(f64, f64)]) -> Clip {
        let mut clip = Clip::blank("c", "T1", ClipKind::Audio, "clip", 0.0, 10.0);
        clip.volume = 0.8;
        clip.keys = points
            .iter()
            .map(|&(at, value)| ClipKey {
                property: KeyProperty::Volume,
                at,
                value,
                ease: KeyEase::LINEAR,
            })
            .collect();
        clip
    }

    /// What a command does to a clip, through the real command layer's
    /// model methods - the same ones `apply` calls.
    fn run(clip: &mut Clip, command: &Command) {
        match command {
            Command::SetClipKey {
                property,
                at,
                value,
                ease,
                ..
            } => clip.set_key(*property, *at, *value, *ease),
            Command::ClearClipKey { property, at, .. } => {
                clip.clear_key(*property, *at);
            }
            Command::UpdateClip { patch, .. } => {
                if let Some(volume) = patch.volume {
                    clip.volume = volume;
                }
            }
            Command::Batch { commands } => {
                for command in commands {
                    run(clip, command);
                }
            }
            other => panic!("unexpected command {other:?}"),
        }
    }

    fn points(clip: &Clip) -> Vec<(f64, f64)> {
        keys(clip).iter().map(|key| (key.at, key.value)).collect()
    }

    #[test]
    fn an_unkeyed_clip_draws_no_envelope() {
        let clip = ride(&[]);
        assert!(samples(&clip).is_empty());
        assert_eq!(path(&samples(&clip)), "");
    }

    #[test]
    fn the_envelope_holds_flat_outside_the_keys() {
        let clip = ride(&[(0.25, 1.0), (0.75, 0.5)]);
        let samples = samples(&clip);
        assert_eq!(samples.first(), Some(&(0.0, 1.0)));
        assert_eq!(samples.last(), Some(&(1.0, 0.5)));
        // Start, first key, the linear stretch's pieces, end.
        assert_eq!(samples.len(), 2 + LINEAR_STEPS + 1);
        // Halfway along a linear stretch is halfway between its values.
        let middle = samples[1 + LINEAR_STEPS / 2];
        assert!((middle.0 - 0.5).abs() < 1e-9);
        assert!((middle.1 - 0.75).abs() < 1e-9);
        assert!(path(&samples).starts_with("M 0.00000 0.50000 L 0.25000 0.50000"));
    }

    #[test]
    fn an_eased_stretch_bends_and_keeps_its_piece_count_while_dragged() {
        let mut clip = ride(&[(0.0, 0.0), (1.0, 2.0)]);
        clip.keys[1].ease = KeyEase::IN;
        let before = samples(&clip);
        assert_eq!(before.len(), 2 + EASED_STEPS + 1);
        // Ease-in starts slow: halfway in time is well under halfway up.
        assert!(before[1 + EASED_STEPS / 2].1 < 0.9);
        // A value dragged changes the shape, never how many pieces it has.
        let original = clip.clone();
        drag(&mut clip, &original, 1, 1.0, 0.3);
        assert_eq!(samples(&clip).len(), before.len());
    }

    #[test]
    fn adding_a_point_keys_the_value_the_line_already_has() {
        // Unkeyed: the constant.
        let clip = ride(&[]);
        let Some(Command::SetClipKey { at, value, .. }) = add(&clip, 0.4) else {
            panic!("a key");
        };
        assert_eq!((at, value), (0.4, 0.8));

        // Keyed: the ride's value there, and the stretch's ease.
        let mut clip = ride(&[(0.0, 0.0), (1.0, 1.0)]);
        clip.keys[1].ease = KeyEase::OUT;
        let command = add(&clip, 0.5).expect("a key");
        let heard = clip.value_at(KeyProperty::Volume, 0.5);
        run(&mut clip, &command);
        assert_eq!(keys(&clip).len(), 3);
        assert!((keys(&clip)[1].value - heard).abs() < 1e-9);
        assert_eq!(keys(&clip)[1].ease, KeyEase::OUT);
    }

    #[test]
    fn adding_on_top_of_a_point_does_nothing() {
        let clip = ride(&[(0.5, 1.0)]);
        assert!(add(&clip, 0.5 + KEY_EPSILON).is_none());
        assert!(add(&clip, f64::NAN).is_none());
    }

    #[test]
    fn a_point_dragged_in_value_lands_as_one_set() {
        let original = ride(&[(0.2, 1.0), (0.8, 1.0)]);
        let mut echo = original.clone();
        drag(&mut echo, &original, 0, 0.2, 0.5);
        let command = commit("c", &original, &echo).expect("a change");
        assert_eq!(
            command,
            Command::SetClipKey {
                clip_id: "c".into(),
                property: KeyProperty::Volume,
                at: 0.2,
                value: 0.5,
                ease: KeyEase::LINEAR,
            }
        );
    }

    #[test]
    fn a_point_dragged_in_time_lands_as_one_batch_of_clear_and_set() {
        let original = ride(&[(0.2, 1.0), (0.8, 0.5)]);
        let mut echo = original.clone();
        drag(&mut echo, &original, 0, 0.4, 1.5);
        let Some(Command::Batch { commands }) = commit("c", &original, &echo) else {
            panic!("one batch");
        };
        assert_eq!(commands.len(), 2);
        assert!(matches!(commands[0], Command::SetClipKey { at, value, .. }
            if (at - 0.4).abs() < 1e-9 && (value - 1.5).abs() < 1e-9));
        assert!(matches!(commands[1], Command::ClearClipKey { at, .. } if at == 0.2));
        let mut clip = original.clone();
        run(&mut clip, &Command::Batch { commands });
        assert_eq!(points(&clip), vec![(0.4, 1.5), (0.8, 0.5)]);
    }

    #[test]
    fn a_dragged_point_stops_short_of_its_neighbours_and_the_band() {
        let original = ride(&[(0.2, 1.0), (0.5, 1.0), (0.8, 1.0)]);
        let (at, gain) = place(&original, 1, 0.95, 9.0).unwrap();
        assert!((at - (0.8 - GAP)).abs() < 1e-9);
        assert_eq!(gain, CEILING);
        let (at, gain) = place(&original, 1, -1.0, -1.0).unwrap();
        assert!((at - (0.2 + GAP)).abs() < 1e-9);
        assert_eq!(gain, 0.0);
        // Unity snaps.
        assert_eq!(place(&original, 1, 0.5, 1.03).unwrap().1, 1.0);
        // The ends are the clip's.
        assert_eq!(place(&original, 0, -0.5, 1.0).unwrap().0, 0.0);
        assert_eq!(place(&original, 2, 1.5, 1.0).unwrap().0, 1.0);
        assert!(place(&original, 3, 0.5, 1.0).is_none());
        // Dragged past a neighbour, the order and the count hold.
        let mut echo = original.clone();
        drag(&mut echo, &original, 0, 0.9, 0.5);
        assert_eq!(keys(&echo).len(), 3);
        assert!(keys(&echo)[0].at < keys(&echo)[1].at);
    }

    #[test]
    fn a_drag_that_comes_back_commits_nothing() {
        let original = ride(&[(0.2, 1.0), (0.8, 0.5)]);
        let mut echo = original.clone();
        drag(&mut echo, &original, 1, 0.6, 0.1);
        drag(&mut echo, &original, 1, 0.8, 0.5);
        assert!(commit("c", &original, &echo).is_none());
    }

    #[test]
    fn removing_a_point_clears_it() {
        let mut clip = ride(&[(0.2, 1.0), (0.8, 0.5)]);
        let command = remove(&clip, 1).expect("a removal");
        assert_eq!(
            command,
            Command::ClearClipKey {
                clip_id: "c".into(),
                property: KeyProperty::Volume,
                at: 0.8,
            }
        );
        run(&mut clip, &command);
        assert_eq!(points(&clip), vec![(0.2, 1.0)]);
        assert!(remove(&clip, 5).is_none());
    }

    #[test]
    fn removing_the_last_point_leaves_the_line_where_it_was() {
        let mut clip = ride(&[(0.5, 1.4)]);
        let command = remove(&clip, 0).expect("a removal");
        assert!(matches!(&command, Command::Batch { commands } if commands.len() == 2));
        run(&mut clip, &command);
        assert!(!clip.is_keyed(KeyProperty::Volume));
        assert_eq!(clip.volume, 1.4);
    }
}
