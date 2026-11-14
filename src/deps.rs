//! プログラムの起動順（task.yaml の `after` と `delay`）．
//!
//! 待つプログラムのペインは，起動せずに置いておき（待機中），
//! 待つ相手がすべて条件を満たし，さらに `delay` が経ったら起動する．

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::Cond;
use crate::layout::PaneId;

/// 各ペインのプログラムが今どうなっているか．
#[derive(Debug, Default, Clone, Copy)]
pub struct RunState {
    /// 一度でも起動したか．
    pub started: bool,
    /// 最後の終了が正常終了だったか（実行中・未終了なら `None`）．
    pub exit: Option<bool>,
}

/// 待つ相手 1 つ．
#[derive(Debug, Clone)]
pub struct Dep {
    pub pane: PaneId,
    pub name: String,
    pub cond: Cond,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DepStatus {
    /// 条件を満たした．
    Met,
    Waiting,
    /// このままでは満たせない（理由）．
    Blocked(&'static str),
}

/// 待機中のペインの情報．
#[derive(Debug)]
pub struct Wait {
    pub deps: Vec<Dep>,
    pub delay: Duration,
    /// すべての相手が条件を満たした時刻（ここから `delay` 待つ）．
    pub ready_since: Option<Instant>,
}

/// 相手 `dep` の状態．`exists` はそのペインがまだ開いているか．
pub fn status(dep: &Dep, runs: &HashMap<PaneId, RunState>, exists: bool) -> DepStatus {
    let run = runs.get(&dep.pane).copied().unwrap_or_default();
    let met = match dep.cond {
        Cond::Started => run.started,
        Cond::Done => run.exit == Some(true),
    };
    if met {
        DepStatus::Met
    } else if dep.cond == Cond::Done && run.exit == Some(false) {
        DepStatus::Blocked("failed")
    } else if !exists {
        DepStatus::Blocked("closed")
    } else {
        DepStatus::Waiting
    }
}

impl Wait {
    /// すべての相手が条件を満たしたか．
    pub fn all_met(&self, runs: &HashMap<PaneId, RunState>, exists: impl Fn(PaneId) -> bool) -> bool {
        self.deps.iter().all(|d| status(d, runs, exists(d.pane)) == DepStatus::Met)
    }

    /// 状態を進め，今起動してよければ true を返す．
    pub fn poll(&mut self, now: Instant, runs: &HashMap<PaneId, RunState>, exists: impl Fn(PaneId) -> bool) -> bool {
        if !self.all_met(runs, exists) {
            self.ready_since = None;
            return false;
        }
        let since = *self.ready_since.get_or_insert(now);
        now >= since + self.delay
    }

    /// 次に起動の判定が必要になる時刻（delay の待ち中のみ）．
    pub fn deadline(&self) -> Option<Instant> {
        self.ready_since.map(|s| s + self.delay)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(pane: PaneId, cond: Cond) -> Dep {
        Dep { pane, name: format!("p{pane}"), cond }
    }

    #[test]
    fn statuses() {
        let mut runs = HashMap::new();
        let d = dep(1, Cond::Done);
        assert_eq!(status(&d, &runs, true), DepStatus::Waiting);
        assert_eq!(status(&d, &runs, false), DepStatus::Blocked("closed"));
        runs.insert(1, RunState { started: true, exit: None });
        assert_eq!(status(&dep(1, Cond::Started), &runs, true), DepStatus::Met);
        assert_eq!(status(&d, &runs, true), DepStatus::Waiting);
        runs.insert(1, RunState { started: true, exit: Some(false) });
        assert_eq!(status(&d, &runs, true), DepStatus::Blocked("failed"));
        runs.insert(1, RunState { started: true, exit: Some(true) });
        // 正常終了したあとでペインを閉じても，条件は満たしたまま
        assert_eq!(status(&d, &runs, false), DepStatus::Met);
    }

    #[test]
    fn delay_after_deps() {
        let mut runs = HashMap::new();
        let mut w = Wait { deps: vec![dep(1, Cond::Started)], delay: Duration::from_secs(2), ready_since: None };
        let t0 = Instant::now();
        assert!(!w.poll(t0, &runs, |_| true));
        assert_eq!(w.deadline(), None);
        runs.insert(1, RunState { started: true, exit: None });
        assert!(!w.poll(t0, &runs, |_| true));
        assert_eq!(w.deadline(), Some(t0 + Duration::from_secs(2)));
        assert!(!w.poll(t0 + Duration::from_secs(1), &runs, |_| true));
        assert!(w.poll(t0 + Duration::from_secs(2), &runs, |_| true));
    }
}
