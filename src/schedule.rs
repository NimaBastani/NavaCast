/// Weekly playlist windows. `days_mask` bit 0 is Sunday.
/// When `end_min <= start_min` the window wraps past midnight.
#[derive(Clone, Debug)]
pub struct Window {
    pub playlist_id: String,
    pub days_mask: i32,
    pub start_min: i32,
    pub end_min: i32,
    pub priority: i32,
    pub enabled: bool,
}

pub fn select(rows: &[Window], unix_secs: i64) -> Option<String> {
    let day = weekday_sun0(unix_secs);
    let minute = minute_of_day(unix_secs);
    let bit = 1i32 << day;
    let mut best: Option<(&Window, i32)> = None;
    for row in rows {
        if !row.enabled || row.days_mask & bit == 0 {
            continue;
        }
        if !contains(row.start_min, row.end_min, minute) {
            continue;
        }
        match best {
            Some((_, pri)) if row.priority <= pri => {}
            _ => best = Some((row, row.priority)),
        }
    }
    best.map(|(r, _)| r.playlist_id.clone())
}

pub fn weekday_sun0(unix_secs: i64) -> i32 {
    let days = unix_secs.div_euclid(86_400);
    (days + 4).rem_euclid(7) as i32
}

pub fn minute_of_day(unix_secs: i64) -> i32 {
    (unix_secs.rem_euclid(86_400) / 60) as i32
}

fn contains(start: i32, end: i32, minute: i32) -> bool {
    if !(0..=24 * 60).contains(&start) || !(0..=24 * 60).contains(&end) {
        return false;
    }
    if start == end {
        return true;
    }
    if end > start {
        minute >= start && minute < end
    } else {
        minute >= start || minute < end
    }
}

pub fn parse_hhmm(s: &str) -> Option<i32> {
    let (h, m) = s.split_once(':')?;
    let h: i32 = h.parse().ok()?;
    let m: i32 = m.parse().ok()?;
    if (0..24).contains(&h) && (0..60).contains(&m) {
        Some(h * 60 + m)
    } else {
        None
    }
}

pub fn parse_days(days: &[String]) -> Option<i32> {
    if days.is_empty() {
        return None;
    }
    let mut mask = 0i32;
    for d in days {
        let bit = match d.to_ascii_lowercase().as_str() {
            "sun" | "sunday" => 0,
            "mon" | "monday" => 1,
            "tue" | "tuesday" => 2,
            "wed" | "wednesday" => 3,
            "thu" | "thursday" => 4,
            "fri" | "friday" => 5,
            "sat" | "saturday" => 6,
            _ => return None,
        };
        mask |= 1 << bit;
    }
    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_and_midnight_wrap() {
        let mut midnight = 1_700_000_000i64;
        midnight -= midnight.rem_euclid(86_400);
        while weekday_sun0(midnight) != 3 {
            midnight += 86_400;
        }
        let wed_2330 = midnight + 23 * 3600 + 30 * 60;
        assert_eq!(weekday_sun0(wed_2330), 3, "expected Wednesday");
        let rows = vec![
            Window { playlist_id: "day".into(), days_mask: 1 << 3, start_min: 8 * 60, end_min: 18 * 60, priority: 1, enabled: true },
            Window { playlist_id: "late".into(), days_mask: 1 << 3, start_min: 22 * 60, end_min: 2 * 60, priority: 2, enabled: true },
            Window { playlist_id: "also".into(), days_mask: 1 << 3, start_min: 23 * 60, end_min: 24 * 60, priority: 1, enabled: true },
            Window { playlist_id: "off".into(), days_mask: 1 << 3, start_min: 0, end_min: 24 * 60, priority: 9, enabled: false },
        ];
        assert_eq!(select(&rows, wed_2330).as_deref(), Some("late"));
        let wed_0900 = wed_2330 - (14 * 60 + 30) * 60;
        assert_eq!(select(&rows, wed_0900).as_deref(), Some("day"));
        let thu_0100 = wed_2330 + 90 * 60;
        assert_eq!(weekday_sun0(thu_0100), 4);
        assert!(select(&rows, thu_0100).is_none());
    }

    #[test]
    fn days_parse() {
        let mask = parse_days(&["mon".into(), "Wed".into()]).unwrap();
        assert_eq!(mask, (1 << 1) | (1 << 3));
        assert!(parse_days(&["nope".into()]).is_none());
        assert_eq!(parse_hhmm("08:30"), Some(8 * 60 + 30));
        assert!(parse_hhmm("24:00").is_none());
    }
}
