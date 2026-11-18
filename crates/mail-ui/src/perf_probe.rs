//! 显式启用的限时诊断：预热 1 秒，采样 5 秒，输出统计并退出。
//! 不把 GPUI 提交完成时间误作合成器呈现反馈，也不改变桌面设置。

use std::time::Duration;

use gpui_kit::{Context, Window, profiler};

use crate::Home;

/// 仅诊断窗口可绕过省电帧率限制，正常运行仍遵循系统电源策略。
pub(super) fn unthrottled() -> bool {
    std::env::var("QINGMAIL_PERF_PROBE")
        .is_ok_and(|mode| matches!(mode.as_str(), "frames" | "theme" | "idle"))
        && std::env::var("QINGMAIL_PERF_UNTHROTTLED").is_ok_and(|value| value == "1")
}

pub(super) fn start(window: &Window, cx: &mut Context<Home>) {
    let Ok(mode) = std::env::var("QINGMAIL_PERF_PROBE") else {
        return;
    };
    if !matches!(mode.as_str(), "frames" | "theme" | "idle") {
        eprintln!("QINGMAIL_PERF_PROBE expects frames, theme or idle");
        return;
    }
    cx.spawn_in(window, async move |entity, cx| {
        cx.background_executor().timer(Duration::from_secs(1)).await;
        let previously_enabled = profiler::trace_enabled();
        profiler::set_trace_enabled(true);
        let mut collector = profiler::FrameTimingCollector::new();
        let mut events = Vec::new();
        let Ok(window_id) = cx.update(|window, cx| {
            eprintln!(
                "PERF mode={mode} unthrottled={} bounds={:?} scale={} active={} reduce_motion={} power={:?} thermal={:?}",
                unthrottled(),
                window.viewport_size(), window.scale_factor(), window.is_window_active(),
                cx.reduce_motion(), cx.power_mode(), cx.thermal_state(),
            );
            if mode == "frames" {
                let _ = entity.update(cx, |home, cx| {
                    home.probe_started = Some(std::time::Instant::now());
                    cx.notify();
                });
            }
            window.window_handle().window_id()
        }) else {
            profiler::set_trace_enabled(previously_enabled);
            return;
        };
        let started = std::time::Instant::now();
        let mut next_toggle = Duration::ZERO;
        let mut previous_state = None;
        let mut draw_bins = [0_u64; 6];
        while started.elapsed() < Duration::from_secs(5) {
            let Ok(state) = cx.update(|window, cx| {
                (window.is_window_active(), cx.power_mode(), cx.reduce_motion())
            }) else {
                profiler::set_trace_enabled(previously_enabled);
                return;
            };
            if previous_state != Some(state) {
                eprintln!("PERF state_at_s={:.3} active={} power={:?} reduce_motion={}", started.elapsed().as_secs_f64(), state.0, state.1, state.2);
                previous_state = Some(state);
            }
            if mode == "theme" && started.elapsed() >= next_toggle {
                if cx.update(|window, cx| {
                    let _ = entity.update(cx, |home, cx| home.toggle_theme(window, cx));
                }).is_err() {
                    profiler::set_trace_enabled(previously_enabled);
                    return;
                }
                next_toggle = started.elapsed() + Duration::from_millis(200);
            }
            cx.background_executor().timer(Duration::from_millis(50)).await;
            let batch = collector.collect_unseen();
            let bin = started.elapsed().as_secs().min(5) as usize;
            draw_bins[bin] += batch.iter().filter(|event| matches!(event, profiler::FrameEvent::Draw(timing) if timing.window_id == window_id)).count() as u64;
            events.extend(batch);
        }
        let elapsed = started.elapsed().as_secs_f64();
        let mut draws = Vec::new();
        let mut submits = Vec::new();
        let mut intervals = Vec::new();
        let mut signal_delays = Vec::new();
        let mut previous_present = None;
        for event in events {
            match event {
                profiler::FrameEvent::Draw(timing) if timing.window_id == window_id => {
                    draws.push(timing.draw_duration().as_secs_f64() * 1000.0);
                    if let Some(signal) = timing.signal_at {
                        signal_delays.push(timing.draw_start.saturating_duration_since(signal).as_secs_f64() * 1000.0);
                    }
                }
                profiler::FrameEvent::Present(timing) if timing.window_id == window_id => {
                    submits.push(timing.present_duration().as_secs_f64() * 1000.0);
                    if let Some(previous) = previous_present {
                        intervals.push(timing.present_end.duration_since(previous).as_secs_f64() * 1000.0);
                    }
                    previous_present = Some(timing.present_end);
                }
                _ => {}
            }
        }
        eprintln!("PERF elapsed_s={elapsed:.3} draws={} submits={} submits_per_s={:.2}", draws.len(), submits.len(), submits.len() as f64 / elapsed);
        summarize("draw_ms", draws);
        summarize("submit_ms", submits);
        summarize("interval_ms", intervals);
        summarize("signal_to_draw_ms", signal_delays);
        eprintln!("PERF draws_by_collection_second={draw_bins:?}");
        profiler::set_trace_enabled(previously_enabled);
        let _ = cx.update(|_, cx| cx.quit());
    }).detach();
}

fn summarize(label: &str, mut values: Vec<f64>) {
    if values.is_empty() {
        eprintln!("PERF {label} samples=0");
        return;
    }
    values.sort_by(f64::total_cmp);
    let percentile = |quantile: f64| values[(values.len() as f64 * quantile).ceil() as usize - 1];
    eprintln!(
        "PERF {label} samples={} mean={:.3} p50={:.3} p95={:.3} max={:.3} over_8ms={} over_16ms={}",
        values.len(),
        values.iter().sum::<f64>() / values.len() as f64,
        percentile(0.5),
        percentile(0.95),
        values[values.len() - 1],
        values.iter().filter(|value| **value > 8.0).count(),
        values.iter().filter(|value| **value > 16.0).count(),
    );
}

/// Opt-in sampling of the actual mailbox window. Observing never requests a frame.
pub(super) fn mailbox_idle<T: 'static>(window: &Window, cx: &mut Context<T>) {
    if std::env::var("QINGMAIL_WINDOW_PERF").as_deref() != Ok("idle") {
        return;
    }
    if std::env::var_os("QINGMAIL_IPC_ROOT").is_none() {
        eprintln!("QINGMAIL_WINDOW_PERF requires isolated QINGMAIL_IPC_ROOT");
        return;
    }
    cx.spawn_in(window, async move |_, cx| {
        // Let initial catalog loading, the first status observation and compositor
        // focus/tiling settle before counting idle frames.
        cx.background_executor().timer(Duration::from_secs(8)).await;
        let enabled = profiler::trace_enabled();
        profiler::set_trace_enabled(true);
        let mut collector = profiler::FrameTimingCollector::new();
        let Ok(id) = cx.update(|window, cx| {
            eprintln!(
                "MAILBOX_PERF scale={} viewport={:?} power={:?} reduce_motion={}",
                window.scale_factor(),
                window.viewport_size(),
                cx.power_mode(),
                cx.reduce_motion()
            );
            window.window_handle().window_id()
        }) else {
            profiler::set_trace_enabled(enabled);
            return;
        };
        let executor = cx.background_executor().clone();
        let sample = cx
            .background_executor()
            .spawn(async move {
                let started = std::time::Instant::now();
                let mut draws = 0;
                let mut submissions = 0;
                for _ in 0..120 {
                    executor.timer(Duration::from_millis(100)).await;
                    for event in collector.collect_unseen() {
                        match event {
                            profiler::FrameEvent::Draw(t) if t.window_id == id => draws += 1,
                            profiler::FrameEvent::Present(t) if t.window_id == id => {
                                submissions += 1
                            }
                            _ => (),
                        }
                    }
                }
                (started.elapsed().as_secs_f64(), draws, submissions)
            })
            .await;
        eprintln!(
            "MAILBOX_PERF idle_seconds={:.3} draws={} submissions={} hud={}",
            sample.0,
            sample.1,
            sample.2,
            crate::show_fps()
        );
        profiler::set_trace_enabled(enabled);
        // Normal lifecycle saves and detaches; do not terminate a daemon under measurement.
        let _ = cx.update(|_, cx| crate::lifecycle::close_frontend(cx));
    })
    .detach();
}
