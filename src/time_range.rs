use chrono::prelude::*;

pub struct TimeRange(pub NaiveTime, pub NaiveTime);

impl TimeRange {
    /// Return Some(local_time) if not within range
    fn check_local(&self) -> (DateTime<Local>, bool) {
        let now = Local::now();
        let now_naive = now.naive_local().time();
        (now, self.contains(now_naive))
    }

    fn contains(&self, t: NaiveTime) -> bool {
        let TimeRange(begin, end) = *self;
        time_greater(end, begin) != (time_greater(t, end) == time_greater(t, begin))
    }
}

fn time_greater(a: NaiveTime, b: NaiveTime) -> bool {
    (a - b).num_milliseconds() > 0
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time() {
        assert!(!
            TimeRange(NaiveTime::from_hms(10, 0, 0), NaiveTime::from_hms(14, 0, 0)).contains(NaiveTime::from_hms(9, 0, 0)),
        );

        assert!(!
            TimeRange(NaiveTime::from_hms(10, 0, 0), NaiveTime::from_hms(14, 0, 0)).contains(NaiveTime::from_hms(15, 0, 0)),
        );

        assert!(
            TimeRange(NaiveTime::from_hms(10, 0, 0), NaiveTime::from_hms(14, 0, 0)).contains(NaiveTime::from_hms(13, 0, 0)),
        );

        assert!(
            TimeRange(NaiveTime::from_hms(14, 0, 0), NaiveTime::from_hms(10, 0, 0)).contains(NaiveTime::from_hms(9, 0, 0)),
        );

        assert!(
            TimeRange(NaiveTime::from_hms(14, 0, 0), NaiveTime::from_hms(10, 0, 0)).contains(NaiveTime::from_hms(15, 0, 0)),
        );

        assert!(!TimeRange(NaiveTime::from_hms(14, 0, 0), NaiveTime::from_hms(10, 0, 0)).contains(NaiveTime::from_hms(13, 0, 0)),
        );
    }
}

