//! In-memory token-bucket rate limiting for HTTP endpoints, keyed by client IP
//! and authenticated principal.

use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_RATE_LIMIT_ENABLED: bool = true;
pub const DEFAULT_IP_REQUESTS_PER_SEC: f64 = 100.0;
pub const DEFAULT_IP_BURST: u32 = 200;
pub const DEFAULT_PRINCIPAL_REQUESTS_PER_SEC: f64 = 30.0;
pub const DEFAULT_PRINCIPAL_BURST: u32 = 60;
pub const DEFAULT_MAX_BUCKETS: usize = 10_000;
const AUDIT_LOG_COOLDOWN: Duration = Duration::from_secs(10);
const MIN_IDLE_EVICT_AGE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub ip_requests_per_sec: f64,
    pub ip_burst: u32,
    pub principal_requests_per_sec: f64,
    pub principal_burst: u32,
    pub trust_proxy_headers: bool,
    pub max_buckets: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_RATE_LIMIT_ENABLED,
            ip_requests_per_sec: DEFAULT_IP_REQUESTS_PER_SEC,
            ip_burst: DEFAULT_IP_BURST,
            principal_requests_per_sec: DEFAULT_PRINCIPAL_REQUESTS_PER_SEC,
            principal_burst: DEFAULT_PRINCIPAL_BURST,
            trust_proxy_headers: false,
            max_buckets: DEFAULT_MAX_BUCKETS,
        }
    }
}

impl RateLimitConfig {
    pub fn validate(&self) -> Result<()> {
        validate_dimension("ip", self.ip_requests_per_sec, self.ip_burst)?;
        validate_dimension(
            "principal",
            self.principal_requests_per_sec,
            self.principal_burst,
        )?;
        if self.max_buckets == 0 {
            bail!("rate_limit.max_buckets must be at least 1");
        }
        Ok(())
    }

    fn ip_policy(&self) -> Option<BucketPolicy> {
        (self.enabled && self.ip_requests_per_sec > 0.0).then_some(BucketPolicy {
            rate_per_sec: self.ip_requests_per_sec,
            capacity: self.ip_burst,
        })
    }

    fn principal_policy(&self) -> Option<BucketPolicy> {
        (self.enabled && self.principal_requests_per_sec > 0.0).then_some(BucketPolicy {
            rate_per_sec: self.principal_requests_per_sec,
            capacity: self.principal_burst,
        })
    }
}

fn validate_dimension(label: &str, rps: f64, burst: u32) -> Result<()> {
    if !rps.is_finite() || rps < 0.0 {
        bail!("rate_limit.{label}_requests_per_sec must be finite and >= 0");
    }
    if rps > 0.0 && burst == 0 {
        bail!("rate_limit.{label}_burst must be at least 1 when {label}_requests_per_sec > 0");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LimitScope {
    Ip,
    Principal,
}

impl LimitScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ip => "ip",
            Self::Principal => "principal",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitHeaders {
    pub scope: LimitScope,
    pub limit: u32,
    pub remaining: u32,
    pub reset_after_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimitRejection {
    pub scope: LimitScope,
    pub limit: u32,
    pub retry_after_secs: u64,
    pub reset_after_secs: u64,
}

#[derive(Clone, Copy, Debug)]
struct BucketPolicy {
    rate_per_sec: f64,
    capacity: u32,
}

#[derive(Clone, Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
    last_seen: Instant,
}

impl BucketState {
    fn refilled_tokens(&self, policy: BucketPolicy, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
        let cap = f64::from(policy.capacity);
        (self.tokens + elapsed * policy.rate_per_sec).min(cap)
    }

    fn refill(&mut self, policy: BucketPolicy, now: Instant) {
        self.tokens = self.refilled_tokens(policy, now);
        self.last_refill = now;
        self.last_seen = now;
    }
}

struct TokenBucketTable<K> {
    scope: LimitScope,
    policy: Option<BucketPolicy>,
    max_buckets: usize,
    buckets: Mutex<HashMap<K, BucketState>>,
}

impl<K: Eq + Hash + Clone> TokenBucketTable<K> {
    fn new(scope: LimitScope, policy: Option<BucketPolicy>, max_buckets: usize) -> Self {
        Self {
            scope,
            policy,
            max_buckets: max_buckets.max(1),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn peek_at(&self, key: &K, now: Instant) -> Result<(), RateLimitRejection> {
        let Some(policy) = self.policy else {
            return Ok(());
        };
        let guard = self.buckets.lock().expect("rate limit bucket lock poisoned");
        let Some(state) = guard.get(key) else {
            return Ok(());
        };
        let tokens = state.refilled_tokens(policy, now);
        if tokens >= 1.0 {
            Ok(())
        } else {
            Err(build_rejection(self.scope, policy, tokens))
        }
    }

    fn acquire_at(&self, key: K, now: Instant) -> Result<Option<RateLimitHeaders>, RateLimitRejection> {
        let Some(policy) = self.policy else {
            return Ok(None);
        };
        let mut guard = self.buckets.lock().expect("rate limit bucket lock poisoned");
        if let Some(state) = guard.get_mut(&key) {
            state.refill(policy, now);
            return consume_from_bucket(self.scope, policy, state).map(Some);
        }

        if guard.len() >= self.max_buckets {
            evict_buckets(&mut guard, policy, self.max_buckets, now);
        }
        let mut state = BucketState {
            tokens: f64::from(policy.capacity),
            last_refill: now,
            last_seen: now,
        };
        let res = consume_from_bucket(self.scope, policy, &mut state).map(Some);
        guard.insert(key, state);
        res
    }
}

fn consume_from_bucket(
    scope: LimitScope,
    policy: BucketPolicy,
    state: &mut BucketState,
) -> Result<RateLimitHeaders, RateLimitRejection> {
    if state.tokens >= 1.0 {
        state.tokens -= 1.0;
        let cap = f64::from(policy.capacity);
        let remaining = state.tokens.floor().max(0.0) as u32;
        let reset_after_secs = ceil_seconds((cap - state.tokens).max(0.0) / policy.rate_per_sec);
        Ok(RateLimitHeaders {
            scope,
            limit: policy.capacity,
            remaining,
            reset_after_secs,
        })
    } else {
        Err(build_rejection(scope, policy, state.tokens))
    }
}

fn build_rejection(scope: LimitScope, policy: BucketPolicy, tokens: f64) -> RateLimitRejection {
    let cap = f64::from(policy.capacity);
    let deficit = (1.0 - tokens).max(0.0);
    let retry_after_secs = ceil_seconds(deficit / policy.rate_per_sec).max(1);
    let reset_after_secs = ceil_seconds((cap - tokens).max(0.0) / policy.rate_per_sec).max(1);
    RateLimitRejection {
        scope,
        limit: policy.capacity,
        retry_after_secs,
        reset_after_secs,
    }
}

fn ceil_seconds(secs: f64) -> u64 {
    if !secs.is_finite() || secs <= 0.0 {
        0
    } else {
        secs.ceil() as u64
    }
}

fn evict_buckets<K: Eq + Hash + Clone>(
    buckets: &mut HashMap<K, BucketState>,
    policy: BucketPolicy,
    max_buckets: usize,
    now: Instant,
) {
    let cap = f64::from(policy.capacity);
    let refill_secs = (cap / policy.rate_per_sec).ceil().max(1.0);
    let idle_ttl = Duration::from_secs_f64((refill_secs * 2.0).max(MIN_IDLE_EVICT_AGE.as_secs_f64()));

    // Prefer dropping buckets that are already full or stale before evicting active entries.
    buckets.retain(|_, state| {
        let idle = now.saturating_duration_since(state.last_seen);
        idle < idle_ttl && state.refilled_tokens(policy, now) < cap
    });

    while buckets.len() >= max_buckets {
        let Some(oldest_key) = buckets
            .iter()
            .min_by_key(|(_, state)| state.last_seen)
            .map(|(k, _)| k.clone())
        else {
            break;
        };
        buckets.remove(&oldest_key);
    }
}

#[derive(Clone, Debug)]
struct AuditLogEntry {
    last_logged: Instant,
    suppressed: u64,
}

#[derive(Clone)]
pub struct RateLimiter {
    config: RateLimitConfig,
    ip_buckets: Arc<TokenBucketTable<IpAddr>>,
    principal_buckets: Arc<TokenBucketTable<String>>,
    audit_log_state: Arc<Mutex<HashMap<(LimitScope, String), AuditLogEntry>>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Result<Self> {
        config.validate()?;
        let max_buckets = config.max_buckets.max(1);
        let ip_buckets = TokenBucketTable::new(LimitScope::Ip, config.ip_policy(), max_buckets);
        let principal_buckets =
            TokenBucketTable::new(LimitScope::Principal, config.principal_policy(), max_buckets);
        Ok(Self {
            config,
            ip_buckets: Arc::new(ip_buckets),
            principal_buckets: Arc::new(principal_buckets),
            audit_log_state: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
            && (self.config.ip_requests_per_sec > 0.0
                || self.config.principal_requests_per_sec > 0.0)
    }

    pub fn trust_proxy_headers(&self) -> bool {
        self.config.trust_proxy_headers
    }

    /// Non-consuming IP check used before expensive credential verification so
    /// an already-throttled IP cannot force Argon2 work with forged tokens.
    pub fn peek_ip(&self, ip: IpAddr) -> Result<(), RateLimitRejection> {
        self.peek_ip_at(ip, Instant::now())
    }

    pub fn peek_ip_at(&self, ip: IpAddr, now: Instant) -> Result<(), RateLimitRejection> {
        if !self.config.enabled {
            return Ok(());
        }
        self.ip_buckets.peek_at(&normalize_ip(ip), now)
    }

    pub fn check_ip(&self, ip: IpAddr) -> Result<Option<RateLimitHeaders>, RateLimitRejection> {
        self.check_ip_at(ip, Instant::now())
    }

    pub fn check_ip_at(
        &self,
        ip: IpAddr,
        now: Instant,
    ) -> Result<Option<RateLimitHeaders>, RateLimitRejection> {
        if !self.config.enabled {
            return Ok(None);
        }
        self.ip_buckets.acquire_at(normalize_ip(ip), now)
    }

    /// Check the principal bucket before the IP bucket so a single throttled
    /// principal on localhost does not drain the shared IP bucket for peers.
    pub fn check_principal_and_ip(
        &self,
        principal_id: &str,
        ip: IpAddr,
    ) -> Result<Option<RateLimitHeaders>, RateLimitRejection> {
        self.check_principal_and_ip_at(principal_id, ip, Instant::now())
    }

    pub fn check_principal_and_ip_at(
        &self,
        principal_id: &str,
        ip: IpAddr,
        now: Instant,
    ) -> Result<Option<RateLimitHeaders>, RateLimitRejection> {
        if !self.config.enabled {
            return Ok(None);
        }
        let principal_headers = self
            .principal_buckets
            .acquire_at(principal_id.to_ascii_lowercase(), now)?;
        let ip_headers = self.ip_buckets.acquire_at(normalize_ip(ip), now)?;
        Ok(match (principal_headers, ip_headers) {
            (Some(p), Some(i)) => {
                if p.remaining <= i.remaining {
                    Some(p)
                } else {
                    Some(i)
                }
            }
            (Some(p), None) => Some(p),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        })
    }

    /// Throttle repeated security-audit log events per `(scope, key)` so a
    /// runaway loop cannot saturate the event forwarder queue.
    pub fn should_log_rejection(&self, scope: LimitScope, key: &str) -> Option<u64> {
        self.should_log_rejection_at(scope, key, Instant::now())
    }

    pub fn should_log_rejection_at(
        &self,
        scope: LimitScope,
        key: &str,
        now: Instant,
    ) -> Option<u64> {
        let mut guard = self
            .audit_log_state
            .lock()
            .expect("rate limit audit log lock poisoned");
        let normalized_key = (scope, key.to_ascii_lowercase());
        if let Some(entry) = guard.get_mut(&normalized_key) {
            if now.saturating_duration_since(entry.last_logged) >= AUDIT_LOG_COOLDOWN {
                let suppressed = entry.suppressed;
                entry.last_logged = now;
                entry.suppressed = 0;
                return Some(suppressed);
            }
            entry.suppressed += 1;
            return None;
        }

        if guard.len() >= self.config.max_buckets {
            guard.retain(|_, entry| {
                now.saturating_duration_since(entry.last_logged) < AUDIT_LOG_COOLDOWN * 6
            });
            while guard.len() >= self.config.max_buckets {
                let Some(oldest) = guard
                    .iter()
                    .min_by_key(|(_, entry)| entry.last_logged)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                guard.remove(&oldest);
            }
        }
        guard.insert(
            normalized_key,
            AuditLogEntry {
                last_logged: now,
                suppressed: 0,
            },
        );
        Some(0)
    }
}

pub fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn test_limiter(ip_rps: f64, ip_burst: u32, p_rps: f64, p_burst: u32) -> RateLimiter {
        RateLimiter::new(RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: ip_rps,
            ip_burst,
            principal_requests_per_sec: p_rps,
            principal_burst: p_burst,
            trust_proxy_headers: false,
            max_buckets: 100,
        })
        .unwrap()
    }

    #[test]
    fn config_validation_rejects_invalid_values() {
        for bad in [
            RateLimitConfig {
                ip_requests_per_sec: -1.0,
                ..Default::default()
            },
            RateLimitConfig {
                ip_requests_per_sec: f64::NAN,
                ..Default::default()
            },
            RateLimitConfig {
                ip_requests_per_sec: 10.0,
                ip_burst: 0,
                ..Default::default()
            },
            RateLimitConfig {
                principal_requests_per_sec: f64::INFINITY,
                ..Default::default()
            },
            RateLimitConfig {
                principal_requests_per_sec: 10.0,
                principal_burst: 0,
                ..Default::default()
            },
            RateLimitConfig {
                max_buckets: 0,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "expected invalid config: {bad:?}");
        }
    }

    #[test]
    fn token_bucket_burst_refill_and_clamp() {
        let limiter = test_limiter(2.0, 3, 0.0, 0);
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let t0 = Instant::now();

        let h1 = limiter.check_ip_at(ip, t0).unwrap().unwrap();
        assert_eq!(h1.remaining, 2);
        let h2 = limiter.check_ip_at(ip, t0).unwrap().unwrap();
        assert_eq!(h2.remaining, 1);
        let h3 = limiter.check_ip_at(ip, t0).unwrap().unwrap();
        assert_eq!(h3.remaining, 0);

        let rej = limiter.check_ip_at(ip, t0).unwrap_err();
        assert_eq!(rej.scope, LimitScope::Ip);
        assert_eq!(rej.limit, 3);
        assert_eq!(rej.retry_after_secs, 1);

        // 400ms at 2 req/s yields 0.8 tokens: still < 1.0, must reject.
        assert!(limiter
            .check_ip_at(ip, t0 + Duration::from_millis(400))
            .is_err());

        // 500ms at 2 req/s yields 1.0 token: allows one more request.
        let h4 = limiter
            .check_ip_at(ip, t0 + Duration::from_millis(500))
            .unwrap()
            .unwrap();
        assert_eq!(h4.remaining, 0);

        // Idle for 60s clamps back to burst capacity (3), never accumulates beyond burst.
        let h5 = limiter
            .check_ip_at(ip, t0 + Duration::from_secs(60))
            .unwrap()
            .unwrap();
        assert_eq!(h5.remaining, 2);
    }

    #[test]
    fn throttled_principal_does_not_drain_shared_localhost_ip_bucket() {
        let limiter = test_limiter(10.0, 10, 5.0, 2);
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let t0 = Instant::now();

        // Agent A consumes its 2 principal tokens (and 2 of 10 localhost IP tokens).
        assert!(limiter.check_principal_and_ip_at("agent-a", localhost, t0).is_ok());
        assert!(limiter.check_principal_and_ip_at("agent-a", localhost, t0).is_ok());

        // Agent A's runaway loop hits 50 more rejections without draining IP tokens.
        for _ in 0..50 {
            let rej = limiter
                .check_principal_and_ip_at("agent-a", localhost, t0)
                .unwrap_err();
            assert_eq!(rej.scope, LimitScope::Principal);
        }

        // Agent B on the same localhost IP still has its own principal tokens AND 8 IP tokens.
        let h = limiter
            .check_principal_and_ip_at("agent-b", localhost, t0)
            .unwrap()
            .unwrap();
        assert_eq!(h.scope, LimitScope::Principal);
        assert_eq!(h.remaining, 1);
    }

    #[test]
    fn ipv4_mapped_ipv6_normalizes_to_ipv4_bucket() {
        let limiter = test_limiter(1.0, 1, 0.0, 0);
        let t0 = Instant::now();
        let v4 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let v6_mapped = IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001));

        assert!(limiter.check_ip_at(v4, t0).is_ok());
        let rej = limiter.check_ip_at(v6_mapped, t0).unwrap_err();
        assert_eq!(rej.scope, LimitScope::Ip);
    }

    #[test]
    fn peek_ip_blocks_exhausted_ip_without_consuming() {
        let limiter = test_limiter(1.0, 1, 10.0, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5));
        let t0 = Instant::now();

        assert!(limiter.peek_ip_at(ip, t0).is_ok());
        assert!(limiter.peek_ip_at(ip, t0).is_ok());
        assert!(limiter.check_ip_at(ip, t0).is_ok());
        assert!(limiter.peek_ip_at(ip, t0).is_err());
    }

    #[test]
    fn bucket_table_evicts_when_max_buckets_reached() {
        let limiter = RateLimiter::new(RateLimitConfig {
            enabled: true,
            ip_requests_per_sec: 10.0,
            ip_burst: 5,
            principal_requests_per_sec: 0.0,
            principal_burst: 0,
            trust_proxy_headers: false,
            max_buckets: 3,
        })
        .unwrap();
        let t0 = Instant::now();

        for i in 1..=10u8 {
            let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, i));
            assert!(limiter
                .check_ip_at(ip, t0 + Duration::from_millis(u64::from(i)))
                .is_ok());
        }
        let guard = limiter.ip_buckets.buckets.lock().unwrap();
        assert!(guard.len() <= 3);
    }

    #[test]
    fn audit_log_cooldown_coalesces_repeated_rejections() {
        let limiter = test_limiter(1.0, 1, 1.0, 1);
        let t0 = Instant::now();

        assert_eq!(
            limiter.should_log_rejection_at(LimitScope::Principal, "agent-a", t0),
            Some(0)
        );
        assert_eq!(
            limiter.should_log_rejection_at(
                LimitScope::Principal,
                "agent-a",
                t0 + Duration::from_secs(2)
            ),
            None
        );
        assert_eq!(
            limiter.should_log_rejection_at(
                LimitScope::Principal,
                "agent-a",
                t0 + Duration::from_secs(5)
            ),
            None
        );
        assert_eq!(
            limiter.should_log_rejection_at(
                LimitScope::Principal,
                "agent-a",
                t0 + Duration::from_secs(11)
            ),
            Some(2)
        );
    }
}
