use std::time::Duration;
use tauri::{PhysicalPosition, WebviewWindow};

const MARGIN: i32 = 16;
const TICK_MS: u64 = 8;

fn win_size(win: &WebviewWindow) -> (i32, i32) {
    win.outer_size()
        .map(|s| (s.width as i32, s.height as i32))
        .unwrap_or((240, 240))
}

/// "Get out of here!" — hop the Bit to the other screen's outer corner:
/// on the right screen → left screen's top-left; otherwise → right screen's
/// top-right. With one screen, toggle between the top corners.
pub fn shoo(win: &WebviewWindow) -> Result<String, String> {
    let monitors = win.available_monitors().map_err(|e| e.to_string())?;
    if monitors.is_empty() {
        return Err("no monitors".into());
    }
    let (ww, _wh) = win_size(win);
    let cur = win.outer_position().map_err(|e| e.to_string())?;
    let center_x = cur.x + ww / 2;

    let mut mons: Vec<&tauri::Monitor> = monitors.iter().collect();
    mons.sort_by_key(|m| m.position().x);

    let target = if mons.len() >= 2 {
        let left = mons.first().unwrap();
        let right = mons.last().unwrap();
        let on_right = center_x >= right.position().x;
        if on_right {
            // left screen, top-left
            PhysicalPosition::new(left.position().x + MARGIN, left.position().y + MARGIN)
        } else {
            // right screen, top-right
            PhysicalPosition::new(
                right.position().x + right.size().width as i32 - ww - MARGIN,
                right.position().y + MARGIN,
            )
        }
    } else {
        let m = mons[0];
        let (mx, my, mw) = (m.position().x, m.position().y, m.size().width as i32);
        let at_left = center_x < mx + mw / 2;
        if at_left {
            PhysicalPosition::new(mx + mw - ww - MARGIN, my + MARGIN)
        } else {
            PhysicalPosition::new(mx + MARGIN, my + MARGIN)
        }
    };

    animate_to(win, cur, target, 30);
    Ok("moved the Bit to the other screen".into())
}

/// Smooth ease-out slide from `from` to `to`.
fn animate_to(win: &WebviewWindow, from: PhysicalPosition<i32>, to: PhysicalPosition<i32>, steps: u32) {
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let e = 1.0 - (1.0 - t).powi(3); // ease-out cubic
        let x = from.x as f64 + (to.x - from.x) as f64 * e;
        let y = from.y as f64 + (to.y - from.y) as f64 * e;
        let _ = win.set_position(PhysicalPosition::new(x.round() as i32, y.round() as i32));
        std::thread::sleep(Duration::from_millis(TICK_MS));
    }
    let _ = win.set_position(to);
}

/// Momentum throw: continue from the release velocity (px/ms), decelerating with
/// friction and bouncing off the outer edges of the combined desktop.
pub fn fling(win: &WebviewWindow, vel: (f64, f64)) {
    let (ww, wh) = win_size(win);
    let monitors = match win.available_monitors() {
        Ok(m) if !m.is_empty() => m,
        _ => return,
    };
    let (mut minx, mut miny, mut maxx, mut maxy) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for m in &monitors {
        let p = m.position();
        let s = m.size();
        minx = minx.min(p.x);
        miny = miny.min(p.y);
        maxx = maxx.max(p.x + s.width as i32);
        maxy = maxy.max(p.y + s.height as i32);
    }
    let max_x = (maxx - ww).max(minx);
    let max_y = (maxy - wh).max(miny);

    let cur = match win.outer_position() {
        Ok(p) => p,
        Err(_) => return,
    };
    let (mut px, mut py) = (cur.x as f64, cur.y as f64);
    let (mut vx, mut vy) = vel;
    let dt = TICK_MS as f64;
    let friction = 0.94;
    let restitution = 0.6;
    let min_speed = 0.03; // px/ms

    loop {
        if (vx * vx + vy * vy).sqrt() < min_speed {
            break;
        }
        px += vx * dt;
        py += vy * dt;
        if px <= minx as f64 {
            px = minx as f64;
            vx = -vx * restitution;
        } else if px >= max_x as f64 {
            px = max_x as f64;
            vx = -vx * restitution;
        }
        if py <= miny as f64 {
            py = miny as f64;
            vy = -vy * restitution;
        } else if py >= max_y as f64 {
            py = max_y as f64;
            vy = -vy * restitution;
        }
        vx *= friction;
        vy *= friction;
        let _ = win.set_position(PhysicalPosition::new(px.round() as i32, py.round() as i32));
        std::thread::sleep(Duration::from_millis(TICK_MS));
    }
}
