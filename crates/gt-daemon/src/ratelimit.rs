use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const READ_LIMIT_PER_MIN: u32 = 600;
pub const WRITE_LIMIT_PER_MIN: u32 = 60;
const WINDOW: Duration = Duration::from_secs(60);
const MAX_TRACKED_IPS: usize = 4096;

struct Window {
    count: u32,
    started: Instant,
}

impl Window {
    fn fresh(now: Instant) -> Self {
        Window {
            count: 0,
            started: now,
        }
    }
}

pub struct RateLimiter {
    windows: Mutex<HashMap<IpAddr, [Window; 2]>>,
    window_len: Duration,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            windows: Mutex::new(HashMap::new()),
            window_len: WINDOW,
        }
    }

    #[cfg(test)]
    fn with_window_len(window_len: Duration) -> Self {
        RateLimiter {
            windows: Mutex::new(HashMap::new()),
            window_len,
        }
    }

    pub fn check(&self, ip: IpAddr, is_write: bool) -> Result<(), u64> {
        let now = Instant::now();
        let mut windows = self.windows.lock().expect("rate limiter lock");
        if !windows.contains_key(&ip) {
            self.make_room(&mut windows, now);
            windows.insert(ip, [Window::fresh(now), Window::fresh(now)]);
        }
        let pair = windows.get_mut(&ip).expect("just inserted");
        let slot = usize::from(is_write);
        let limit = if is_write {
            WRITE_LIMIT_PER_MIN
        } else {
            READ_LIMIT_PER_MIN
        };
        let window = &mut pair[slot];
        if now.duration_since(window.started) >= self.window_len {
            window.started = now;
            window.count = 0;
        }
        if window.count >= limit {
            let retry_after = self
                .window_len
                .checked_sub(now.duration_since(window.started))
                .map(|d| d.as_secs().max(1))
                .unwrap_or(1);
            return Err(retry_after);
        }
        window.count += 1;
        Ok(())
    }

    fn make_room(&self, windows: &mut HashMap<IpAddr, [Window; 2]>, now: Instant) {
        if windows.len() < MAX_TRACKED_IPS {
            return;
        }
        windows.retain(|_, pair| {
            pair.iter()
                .any(|w| now.duration_since(w.started) < self.window_len)
        });
        if windows.len() >= MAX_TRACKED_IPS {
            let drop_count = windows.len() / 2;
            let victims: Vec<IpAddr> = windows.keys().take(drop_count).copied().collect();
            for ip in victims {
                windows.remove(&ip);
            }
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
    }

    #[test]
    fn allows_up_to_read_limit_then_rejects() {
        let limiter = RateLimiter::new();
        for _ in 0..READ_LIMIT_PER_MIN {
            assert!(limiter.check(test_ip(), false).is_ok());
        }
        let retry = limiter.check(test_ip(), false).unwrap_err();
        assert!((1..=60).contains(&retry));
    }

    #[test]
    fn write_bucket_is_separate_and_stricter() {
        let limiter = RateLimiter::new();
        for _ in 0..WRITE_LIMIT_PER_MIN {
            assert!(limiter.check(test_ip(), true).is_ok());
        }
        assert!(limiter.check(test_ip(), true).is_err());
        assert!(limiter.check(test_ip(), false).is_ok());
    }

    #[test]
    fn window_reset_allows_again() {
        let limiter = RateLimiter::with_window_len(Duration::from_millis(20));
        for _ in 0..WRITE_LIMIT_PER_MIN {
            assert!(limiter.check(test_ip(), true).is_ok());
        }
        assert!(limiter.check(test_ip(), true).is_err());
        std::thread::sleep(Duration::from_millis(30));
        assert!(limiter.check(test_ip(), true).is_ok());
    }

    #[test]
    fn different_ips_are_tracked_independently() {
        let limiter = RateLimiter::new();
        let other = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        for _ in 0..WRITE_LIMIT_PER_MIN {
            assert!(limiter.check(test_ip(), true).is_ok());
        }
        assert!(limiter.check(test_ip(), true).is_err());
        assert!(limiter.check(other, true).is_ok());
    }

    #[test]
    fn tracked_ips_stay_bounded() {
        let limiter = RateLimiter::new();
        for i in 0..(MAX_TRACKED_IPS + 500) {
            let ip = IpAddr::V4(Ipv4Addr::new(
                10,
                ((i >> 16) & 0xff) as u8,
                ((i >> 8) & 0xff) as u8,
                (i & 0xff) as u8,
            ));
            let _ = limiter.check(ip, false);
        }
        assert!(limiter.windows.lock().unwrap().len() <= MAX_TRACKED_IPS);
    }
}
