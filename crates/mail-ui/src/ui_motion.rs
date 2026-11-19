//! Shared M3E motion. Sample only while drawing; GPUI schedules frames only
//! until a channel settles, and keeps its system power-saving frame policy.
use gpui_kit::{
    Animation, AnimationExt, AnyElement, App, ElementId, IntoElement, RenderOnce, Styled,
    ViewElement, Window, base, px,
};
use gpui_kit_material::{motion, prelude::ColorRole, theme};
pub(crate) use motion::MotionToken;

pub(crate) fn reduced(cx: &App) -> bool {
    crate::settings::prefs(cx).reduce_motion || cx.reduce_motion()
}

fn follow<T: base::Interpolate + PartialEq + 'static>(
    id: impl Into<ElementId>,
    target: T,
    token: MotionToken,
    window: &mut Window,
    cx: &mut App,
) -> T {
    if reduced(cx) {
        // Sample the endpoint too: re-enabling motion must not resume an old
        // selection or disclosure. Keep the same channel identity and type.
        base::transition(
            id.into(),
            target,
            base::Transition::new(std::time::Duration::ZERO),
            window,
            cx,
        )
    } else {
        motion::token_transition(id, target, token, window, cx)
    }
}

pub(crate) fn value(
    id: impl Into<ElementId>,
    target: f32,
    token: MotionToken,
    window: &mut Window,
    cx: &mut App,
) -> f32 {
    let id = id.into();
    let sampled = follow(id.clone(), target, token, window, cx);
    static PROBE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *PROBE.get_or_init(|| {
        std::env::var_os("QINGMAIL_MOTION_PROBE").is_some()
            && std::env::var_os("QINGMAIL_IPC_ROOT").is_some()
    }) && format!("{id:?}").contains("-height")
    {
        eprintln!(
            "MOTION_SAMPLE target={target} value={sampled:.3} reduced={}",
            reduced(cx)
        );
    }
    sampled
}

pub(crate) fn color(
    id: impl Into<ElementId>,
    target: ColorRole,
    window: &mut Window,
    cx: &mut App,
) -> gpui_kit::Hsla {
    follow(
        id,
        theme::color(target).into(),
        MotionToken::FastEffects,
        window,
        cx,
    )
}

/// Use logical destination IDs, never save revisions, polling results or time.
pub(crate) fn page<E: IntoElement + Styled + 'static>(
    id: impl Into<ElementId>,
    view: E,
    cx: &App,
) -> AnyElement {
    entrance(id, view, 12., cx)
}

pub(crate) fn enter<E: IntoElement + Styled + 'static>(
    id: impl Into<ElementId>,
    view: E,
    cx: &App,
) -> AnyElement {
    // Keep form fields stationary to preserve pointer targeting and IME.
    entrance(id, view, 0., cx)
}

fn entrance<E: IntoElement + Styled + 'static>(
    id: impl Into<ElementId>,
    view: E,
    distance: f32,
    cx: &App,
) -> AnyElement {
    ViewElement::new(Entrance {
        id: id.into(),
        view,
        distance,
        reduced: reduced(cx),
    })
    .into_any_element()
}

struct Entrance<E> {
    id: ElementId,
    view: E,
    distance: f32,
    reduced: bool,
}
impl<E: IntoElement + Styled + 'static> RenderOnce for Entrance<E> {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Preserve the wrapper when the preference changes. Once skipped,
        // this mounted entrance must not replay when motion is re-enabled.
        let state =
            window.use_keyed_state((self.id.clone(), "qingmail-entrance"), cx, |_, _| false);
        let completed = if self.reduced || reduced(cx) {
            state.update(cx, |done, _| {
                *done = true;
            });
            true
        } else {
            *state.read(cx)
        };
        if completed {
            return self.view.into_any_element();
        }
        let effects = motion::curve(MotionToken::DefaultEffects);
        let spatial = motion::curve(MotionToken::DefaultSpatial);
        let duration = if self.distance == 0. {
            effects.duration()
        } else {
            effects.duration().max(spatial.duration())
        };
        let seconds = duration.as_secs_f32();
        let animation = Animation::new(duration);
        self.view
            .with_animation(self.id, animation, move |view, t| {
                let opacity = effects.ease()(t * seconds / effects.duration().as_secs_f32());
                let offset = spatial.ease()(t * seconds / spatial.duration().as_secs_f32());
                view.opacity(opacity.clamp(0., 1.))
                    .relative()
                    .top(px(self.distance * (1. - offset)))
            })
            .into_any_element()
    }
}
