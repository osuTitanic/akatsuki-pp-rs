//! Every aspect of osu!'s pp calculation is being used.
//! This should result in the most accurate values but with
//! drawback of being slower than `osu_fast`.

#![cfg(feature = "osu_precise")]

use std::mem;

mod difficulty_object;
mod osu_object;
mod skill;
mod skill_kind;
mod slider_state;

use difficulty_object::DifficultyObject;
use osu_object::{ObjectParameters, OsuObject};
use skill::Skill;
use skill_kind::SkillKind;
use slider_state::SliderState;

use crate::{curve::CurveBuffers, parse::Pos2, Beatmap, Mods, Strains};

use super::DifficultyAttributes;

const OBJECT_RADIUS: f32 = 64.0;
const SECTION_LEN: f32 = 400.0;
const DIFFICULTY_MULTIPLIER: f32 = 0.0675;
const NORMALIZED_RADIUS: f32 = 52.0;
const STACK_DISTANCE: f32 = 3.0;

/// Star calculation for osu!standard maps.
///
/// Slider paths aswell as stack leniency are considered.
/// Both of these drag the performance down but in turn the values are much more accurate
///
/// In case of a partial play, e.g. a fail, one can specify the amount of passed objects.
pub fn stars(
    map: &Beatmap,
    mods: impl Mods,
    passed_objects: Option<usize>,
) -> DifficultyAttributes {
    let take = passed_objects.unwrap_or_else(|| map.hit_objects.len());

    let map_attributes = map.attributes().mods(mods);
    let hit_window = super::difficulty_range_od(map_attributes.od) / map_attributes.clock_rate;
    let od = (80.0 - hit_window) / 6.0;

    if take < 2 {
        return DifficultyAttributes {
            ar: map_attributes.ar,
            hp: map_attributes.hp,
            od,
            ..Default::default()
        };
    }

    let mut raw_ar = map.ar;
    let hr = mods.hr();

    if hr {
        raw_ar = (raw_ar * 1.4).min(10.0);
    } else if mods.ez() {
        raw_ar *= 0.5;
    }

    let time_preempt = difficulty_range_ar(raw_ar);
    let scale = (1.0 - 0.7 * (map_attributes.cs - 5.0) / 5.0) / 2.0;
    let radius = OBJECT_RADIUS * scale;
    let mut scaling_factor = NORMALIZED_RADIUS / radius;

    if radius < 30.0 {
        let small_circle_bonus = (30.0 - radius).min(5.0) / 50.0;
        scaling_factor *= 1.0 + small_circle_bonus;
    }

    let mut params = ObjectParameters {
        map,
        radius,
        scaling_factor,
        max_combo: 0,
        slider_state: SliderState::new(map),
        ticks: Vec::new(),
        curve_bufs: CurveBuffers::default(),
    };

    let hit_objects_iter = map
        .hit_objects
        .iter()
        .take(take)
        .filter_map(|h| OsuObject::new(h, hr, &mut params));

    let mut hit_objects = Vec::with_capacity(take);
    hit_objects.extend(hit_objects_iter);

    let stack_threshold = time_preempt * map.stack_leniency;

    if map.version >= 6 {
        stacking(&mut hit_objects, stack_threshold);
    } else {
        old_stacking(&mut hit_objects, stack_threshold);
    }

    let scale_factor = scale * -6.4;

    let mut hit_objects = hit_objects.into_iter().map(|mut h| {
        let stack_offset = h.stack_height * scale_factor;

        h.time /= map_attributes.clock_rate;
        h.pos += Pos2::new(stack_offset);

        h
    });

    let fl = mods.fl();
    let mut skills = Vec::with_capacity(2 + fl as usize);

    skills.push(Skill::new(SkillKind::Aim));
    skills.push(Skill::new(SkillKind::speed(hit_window)));

    if fl {
        skills.push(Skill::new(SkillKind::flashlight(scaling_factor)));
    }

    let mut prev_prev = None;
    let mut prev = hit_objects.next().unwrap();
    let mut prev_vals = None;

    // First object has no predecessor and thus no strain, handle distinctly
    let mut current_section_end = (prev.time / SECTION_LEN).ceil() * SECTION_LEN;

    // Handle second object separately to remove later if-branching
    let curr = hit_objects.next().unwrap();
    let h = DifficultyObject::new(
        &curr,
        &prev,
        prev_vals,
        prev_prev,
        scale_factor,
        scaling_factor,
    );

    while h.base.time > current_section_end {
        for skill in skills.iter_mut() {
            skill.start_new_section_from(current_section_end);
        }

        current_section_end += SECTION_LEN;
    }

    for skill in skills.iter_mut() {
        skill.process(&h);
    }

    prev_prev = Some(prev);
    prev_vals = Some((h.jump_dist, h.strain_time));
    prev = curr;

    // Handle all other objects
    for curr in hit_objects {
        let h = DifficultyObject::new(
            &curr,
            &prev,
            prev_vals,
            prev_prev,
            scale_factor,
            scaling_factor,
        );

        while h.base.time > current_section_end {
            for skill in skills.iter_mut() {
                skill.save_current_peak();
                skill.start_new_section_from(current_section_end);
            }

            current_section_end += SECTION_LEN;
        }

        for skill in skills.iter_mut() {
            skill.process(&h);
        }

        prev_prev = Some(prev);
        prev_vals = Some((h.jump_dist, h.strain_time));
        prev = curr;
    }

    for skill in skills.iter_mut() {
        skill.save_current_peak();
    }

    let aim_rating = skills[0].difficulty_value().sqrt() * DIFFICULTY_MULTIPLIER;

    let speed_rating = if mods.rx() {
        0.0
    } else {
        skills[1].difficulty_value().sqrt() * DIFFICULTY_MULTIPLIER
    };

    let flashlight_rating = skills.get_mut(2).map_or(0.0, |skill| {
        skill.difficulty_value().sqrt() * DIFFICULTY_MULTIPLIER
    });

    let base_aim_performance = {
        let base = 5.0 * (aim_rating / 0.0675).max(1.0) - 4.0;

        base * base * base / 100_000.0
    };

    let base_speed_performance = {
        let base = 5.0 * (speed_rating / 0.0675).max(1.0) - 4.0;

        base * base * base / 100_000.0
    };

    let base_flashlight_performance = if fl {
        flashlight_rating * flashlight_rating * 25.0
    } else {
        0.0
    };

    let base_performance = (base_aim_performance.powf(1.1)
        + base_speed_performance.powf(1.1)
        + base_flashlight_performance.powf(1.1))
    .powf(1.0 / 1.1);

    let star_rating = if base_performance > 0.00001 {
        1.12_f32.cbrt()
            * 0.027
            * ((100_000.0 / (1.0_f32 / 1.1).exp2() * base_performance).cbrt() + 4.0)
    } else {
        0.0
    };

    DifficultyAttributes {
        ar: map_attributes.ar,
        hp: map_attributes.hp,
        od,
        aim_strain: aim_rating,
        speed_strain: speed_rating,
        flashlight_rating,
        n_circles: map.n_circles as usize,
        n_sliders: map.n_sliders as usize,
        n_spinners: map.n_spinners as usize,
        stars: star_rating,
        max_combo: params.max_combo,
    }
}

/// Essentially the same as the `stars` function but instead of
/// evaluating the final strains, it just returns them as is.
///
/// Suitable to plot the difficulty of a map over time.
pub fn strains(map: &Beatmap, mods: impl Mods) -> Strains {
    let map_attributes = map.attributes().mods(mods);
    let hit_window = super::difficulty_range_od(map_attributes.od) / map_attributes.clock_rate;

    if map.hit_objects.len() < 2 {
        return Strains::default();
    }

    let mut raw_ar = map.ar;
    let hr = mods.hr();

    if hr {
        raw_ar *= 1.4;
    } else if mods.ez() {
        raw_ar *= 0.5;
    }

    let time_preempt = difficulty_range_ar(raw_ar);
    let scale = (1.0 - 0.7 * (map_attributes.cs - 5.0) / 5.0) / 2.0;
    let radius = OBJECT_RADIUS * scale;
    let mut scaling_factor = NORMALIZED_RADIUS / radius;

    if radius < 30.0 {
        let small_circle_bonus = (30.0 - radius).min(5.0) / 50.0;
        scaling_factor *= 1.0 + small_circle_bonus;
    }

    let mut params = ObjectParameters {
        map,
        radius,
        scaling_factor,
        max_combo: 0,
        slider_state: SliderState::new(map),
        ticks: Vec::new(),
        curve_bufs: CurveBuffers::default(),
    };

    let hit_objects_iter = map
        .hit_objects
        .iter()
        .filter_map(|h| OsuObject::new(h, hr, &mut params));

    let mut hit_objects = Vec::with_capacity(map.hit_objects.len());
    hit_objects.extend(hit_objects_iter);

    let stack_threshold = time_preempt * map.stack_leniency;

    if map.version >= 6 {
        stacking(&mut hit_objects, stack_threshold);
    } else {
        old_stacking(&mut hit_objects, stack_threshold);
    }

    let scale_factor = scale * -6.4;

    let mut hit_objects = hit_objects.into_iter().map(|mut h| {
        let stack_offset = h.stack_height * scale_factor;

        h.time /= map_attributes.clock_rate;
        h.pos += Pos2::new(stack_offset);

        h
    });

    let fl = mods.fl();
    let mut skills = Vec::with_capacity(2 + fl as usize);

    skills.push(Skill::new(SkillKind::Aim));
    skills.push(Skill::new(SkillKind::speed(hit_window)));

    if fl {
        skills.push(Skill::new(SkillKind::flashlight(scaling_factor)));
    }

    let mut prev_prev = None;
    let mut prev = hit_objects.next().unwrap();
    let mut prev_vals = None;

    // First object has no predecessor and thus no strain, handle distinctly
    let mut current_section_end = (prev.time / SECTION_LEN).ceil() * SECTION_LEN;

    // Handle second object separately to remove later if-branching
    let curr = hit_objects.next().unwrap();
    let h = DifficultyObject::new(
        &curr,
        &prev,
        prev_vals,
        prev_prev,
        scale_factor,
        scaling_factor,
    );

    while h.base.time > current_section_end {
        for skill in skills.iter_mut() {
            skill.start_new_section_from(current_section_end);
        }

        current_section_end += SECTION_LEN;
    }

    for skill in skills.iter_mut() {
        skill.process(&h);
    }

    prev_prev = Some(prev);
    prev_vals = Some((h.jump_dist, h.strain_time));
    prev = curr;

    // Handle all other objects
    for curr in hit_objects {
        let h = DifficultyObject::new(
            &curr,
            &prev,
            prev_vals,
            prev_prev,
            scale_factor,
            scaling_factor,
        );

        while h.base.time > current_section_end {
            for skill in skills.iter_mut() {
                skill.save_current_peak();
                skill.start_new_section_from(current_section_end);
            }

            current_section_end += SECTION_LEN;
        }

        for skill in skills.iter_mut() {
            skill.process(&h);
        }

        prev_prev = Some(prev);
        prev_vals = Some((h.jump_dist, h.strain_time));
        prev = curr;
    }

    for skill in skills.iter_mut() {
        skill.save_current_peak();
    }

    let mut speed_strains = skills.pop().unwrap().strain_peaks;
    let mut aim_strains = skills.pop().unwrap().strain_peaks;

    let strains = if let Some(mut flashlight_strains) = skills.pop().map(|s| s.strain_peaks) {
        mem::swap(&mut speed_strains, &mut aim_strains);
        mem::swap(&mut aim_strains, &mut flashlight_strains);

        aim_strains
            .into_iter()
            .zip(speed_strains)
            .zip(flashlight_strains)
            .map(|((aim, speed), flashlight)| aim + speed + flashlight)
            .collect()
    } else {
        aim_strains
            .into_iter()
            .zip(speed_strains)
            .map(|(aim, speed)| aim + speed)
            .collect()
    };

    Strains {
        section_length: SECTION_LEN,
        strains,
    }
}

fn stacking(hit_objects: &mut [OsuObject], stack_threshold: f32) {
    let mut extended_start_idx = 0;
    let extended_end_idx = hit_objects.len() - 1;

    // First big `if` in osu!lazer's function can be skipped

    for i in (1..=extended_end_idx).rev() {
        let mut n = i;
        let mut obj_i_idx = i;
        // * We should check every note which has not yet got a stack.
        // * Consider the case we have two interwound stacks and this will make sense.
        // *   o <-1      o <-2
        // *    o <-3      o <-4
        // * We first process starting from 4 and handle 2,
        // * then we come backwards on the i loop iteration until we reach 3 and handle 1.
        // * 2 and 1 will be ignored in the i loop because they already have a stack value.

        if hit_objects[obj_i_idx].stack_height.abs() > 0.0 || hit_objects[obj_i_idx].is_spinner() {
            continue;
        }

        // * If this object is a hitcircle, then we enter this "special" case.
        // * It either ends with a stack of hitcircles only,
        // * or a stack of hitcircles that are underneath a slider.
        // * Any other case is handled by the "is_slider" code below this.
        if hit_objects[obj_i_idx].is_circle() {
            loop {
                n = match n.checked_sub(1) {
                    Some(n) => n,
                    None => break,
                };

                if hit_objects[n].is_spinner() {
                    continue;
                } else if hit_objects[obj_i_idx].time - hit_objects[n].end_time() > stack_threshold
                {
                    break; // * We are no longer within stacking range of the previous object.
                }

                // * HitObjects before the specified update range haven't been reset yet
                if n < extended_start_idx {
                    hit_objects[n].stack_height = 0.0;
                    extended_start_idx = n;
                }

                // * This is a special case where hticircles are moved DOWN and RIGHT (negative stacking)
                // * if they are under the *last* slider in a stacked pattern.
                // *    o==o <- slider is at original location
                // *        o <- hitCircle has stack of -1
                // *         o <- hitCircle has stack of -2
                if hit_objects[n].is_slider()
                    && hit_objects[n]
                        .end_pos()
                        .distance(hit_objects[obj_i_idx].pos)
                        < STACK_DISTANCE
                {
                    let offset =
                        hit_objects[obj_i_idx].stack_height - hit_objects[n].stack_height + 1.0;

                    for j in n + 1..=i {
                        // * For each object which was declared under this slider, we will offset
                        // * it to appear *below* the slider end (rather than above).
                        if hit_objects[n].end_pos().distance(hit_objects[j].pos) < STACK_DISTANCE {
                            hit_objects[j].stack_height -= offset;
                        }
                    }

                    // * We have hit a slider. We should restart calculation using this as the new base.
                    // * Breaking here will mean that the slider still has StackCount of 0,
                    // * so will be handled in the i-outer-loop.
                    break;
                }

                if hit_objects[n].pos.distance(hit_objects[obj_i_idx].pos) < STACK_DISTANCE {
                    // * Keep processing as if there are no sliders.
                    // * If we come across a slider, this gets cancelled out.
                    // * NOTE: Sliders with start positions stacking
                    // * are a special case that is also handled here.

                    hit_objects[n].stack_height = hit_objects[obj_i_idx].stack_height + 1.0;
                    obj_i_idx = n;
                }
            }
        } else if hit_objects[obj_i_idx].is_slider() {
            // * We have hit the first slider in a possible stack.
            // * From this point on, we ALWAYS stack positive regardless.
            loop {
                n = match n.checked_sub(1) {
                    Some(n) => n,
                    None => break,
                };

                if hit_objects[n].is_spinner() {
                    continue;
                } else if hit_objects[obj_i_idx].time - hit_objects[n].time > stack_threshold {
                    break; // * We are no longer within stacking range of the previous object.
                }

                if hit_objects[n]
                    .end_pos()
                    .distance(hit_objects[obj_i_idx].pos)
                    < STACK_DISTANCE
                {
                    hit_objects[n].stack_height = hit_objects[obj_i_idx].stack_height + 1.0;
                    obj_i_idx = n;
                }
            }
        }
    }
}

fn old_stacking(hit_objects: &mut [OsuObject], stack_threshold: f32) {
    for i in 0..hit_objects.len() {
        if hit_objects[i].stack_height != 0.0 && !hit_objects[i].is_slider() {
            continue;
        }

        let mut start_time = hit_objects[i].end_time();
        let end_pos = hit_objects[i].end_pos();

        let mut slider_stack = 0.0;

        for j in i + 1..hit_objects.len() {
            if hit_objects[j].time - stack_threshold > start_time {
                break;
            }

            if hit_objects[j].pos.distance(hit_objects[i].pos) < STACK_DISTANCE {
                hit_objects[i].stack_height += 1.0;
                start_time = hit_objects[j].end_time();
            } else if hit_objects[j].pos.distance(end_pos) < STACK_DISTANCE {
                slider_stack += 1.0;
                hit_objects[j].stack_height -= slider_stack;
                start_time = hit_objects[j].end_time();
            }
        }
    }
}

const OSU_AR_MAX: f32 = 450.0;
const OSU_AR_AVG: f32 = 1200.0;
const OSU_AR_MIN: f32 = 1800.0;

#[inline]
fn difficulty_range_ar(ar: f32) -> f32 {
    crate::difficulty_range(ar, OSU_AR_MAX, OSU_AR_AVG, OSU_AR_MIN)
}
