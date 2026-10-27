//! Bitrate adattivo in base alla latenza osservata dal viewer.
//!
//! Quando la rete è satura i pacchetti si accodano (nei router, nel buffer di
//! QUIC) e la latenza cattura→visualizzazione sale ben sopra il suo minimo.
//! Il controllore confronta la latenza mediana recente con la minima degli
//! ultimi 10 secondi (la latenza "a rete vuota"):
//! - coda sopra 60 ms → bitrate × 0,8 (al massimo una volta al secondo);
//! - coda sotto 25 ms da un po' → bitrate × 1,08 (risale piano).
//!
//! Le soglie sono larghe di proposito: su un collegamento lento anche un frame
//! più grande impiega di più a passare, e una ritrasmissione occasionale alza
//! la latenza di un RTT; nessuna delle due è una coda da smaltire.
//!
//! Se i frame partono ma non arriva feedback per oltre 1,5 s, la rete è
//! probabilmente bloccata: si scende come per una coda alta.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const QUEUE_HIGH_MS: f64 = 60.0;
const QUEUE_LOW_MS: f64 = 25.0;
const DECREASE: f64 = 0.8;
const INCREASE: f64 = 1.08;
const DECREASE_INTERVAL: Duration = Duration::from_secs(1);
const INCREASE_AFTER_DECREASE: Duration = Duration::from_secs(3);
const INCREASE_INTERVAL: Duration = Duration::from_secs(1);
const BASE_WINDOW: Duration = Duration::from_secs(10);
/// Dopo un cambio di bitrate le misure si ignorano per un po': l'encoder (che per
/// Quick Sync va riavviato) e la coda di rete hanno bisogno di assestarsi, e il
/// picco di latenza del cambio non va scambiato per congestione.
const SETTLE: Duration = Duration::from_millis(1500);
pub const FEEDBACK_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct Abr {
    bitrate: f64,
    min: f64,
    max: f64,
    /// Latenze minime riportate dal viewer, per la base "a rete vuota".
    mins: VecDeque<(Instant, f64)>,
    last_decrease: Option<Instant>,
    last_increase: Instant,
    settle_until: Option<Instant>,
}

impl Abr {
    pub fn new(start_bps: u32, min_bps: u32, max_bps: u32) -> Self {
        Self {
            bitrate: start_bps.clamp(min_bps, max_bps) as f64,
            min: min_bps as f64,
            max: max_bps as f64,
            mins: VecDeque::new(),
            last_decrease: None,
            last_increase: Instant::now(),
            settle_until: None,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate as u32
    }

    /// Nuovo tetto (cambio di qualità). Restituisce il nuovo bitrate se cambia.
    pub fn set_max(&mut self, max_bps: u32) -> Option<u32> {
        self.max = (max_bps as f64).max(self.min);
        self.set(self.bitrate.min(self.max))
    }

    /// Nuovo feedback: latenza mediana e minima dell'ultimo intervallo (ms).
    /// Restituisce il nuovo bitrate se cambia.
    pub fn on_feedback(&mut self, now: Instant, p50_ms: f64, min_ms: f64) -> Option<u32> {
        if self.settle_until.is_some_and(|t| now < t) {
            return None;
        }
        self.mins.push_back((now, min_ms));
        while self.mins.front().is_some_and(|(t, _)| now.duration_since(*t) > BASE_WINDOW) {
            self.mins.pop_front();
        }
        let base = self.mins.iter().map(|(_, m)| *m).fold(f64::INFINITY, f64::min);
        let queue = p50_ms - base;
        if queue > QUEUE_HIGH_MS {
            return self.decrease(now);
        }
        let calm = self.last_decrease.is_none_or(|t| now.duration_since(t) > INCREASE_AFTER_DECREASE);
        if queue < QUEUE_LOW_MS && calm && now.duration_since(self.last_increase) > INCREASE_INTERVAL {
            self.last_increase = now;
            return self.set(self.bitrate * INCREASE);
        }
        None
    }

    /// Nessun feedback mentre si inviano frame: si tratta come una coda alta.
    pub fn on_silence(&mut self, now: Instant) -> Option<u32> {
        self.decrease(now)
    }

    fn decrease(&mut self, now: Instant) -> Option<u32> {
        if self.last_decrease.is_some_and(|t| now.duration_since(t) < DECREASE_INTERVAL) {
            return None;
        }
        self.last_decrease = Some(now);
        self.set_at(now, self.bitrate * DECREASE)
    }

    fn set(&mut self, bps: f64) -> Option<u32> {
        self.set_at(Instant::now(), bps)
    }

    fn set_at(&mut self, now: Instant, bps: f64) -> Option<u32> {
        let new = bps.clamp(self.min, self.max);
        if (new - self.bitrate).abs() < 1.0 {
            return None;
        }
        self.bitrate = new;
        self.settle_until = Some(now + SETTLE);
        Some(new as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decreases_on_queue_and_recovers() {
        let t0 = Instant::now();
        let mut abr = Abr::new(4_000_000, 300_000, 8_000_000);
        // Rete libera: latenza stabile a 40 ms, il bitrate sale.
        assert_eq!(abr.on_feedback(t0, 40.0, 35.0), None);
        let t = |ms| t0 + Duration::from_millis(ms);
        let up = abr.on_feedback(t(1100), 40.0, 35.0);
        assert_eq!(up, Some(4_320_000));
        // Durante l'assestamento (1,5 s) le misure non contano, nemmeno un picco.
        assert_eq!(abr.on_feedback(t(1300), 500.0, 400.0), None);
        // Coda: la mediana sale a 120 ms con minimo storico 35 → scende.
        let down = abr.on_feedback(t(2700), 120.0, 100.0);
        assert_eq!(down, Some(3_456_000));
        // Dopo l'assestamento, di nuovo giù se la coda resta.
        assert!(abr.on_feedback(t(4300), 120.0, 100.0).is_some());
        // Dopo la discesa niente salite per 3 s anche se la coda si svuota.
        assert_eq!(abr.on_feedback(t(6000), 40.0, 35.0), None);
        assert!(abr.on_feedback(t(7400), 40.0, 35.0).is_some());
    }

    #[test]
    fn respects_limits() {
        let t0 = Instant::now();
        let mut abr = Abr::new(550_000, 500_000, 8_000_000);
        abr.on_silence(t0);
        assert_eq!(abr.bitrate(), 500_000);
        assert_eq!(abr.on_silence(t0 + Duration::from_secs(1)), None);
    }
}
