//! Pure helpers behind the stats view:
//!
//! * peak-day formatting ([`format_peak_day`])
//! * the date-range vocabulary and its cycling order
//! * the book / time comparison tables and the fun-factoid text
//! * the shot-distribution summary
//! * token-chart preparation and its x-axis label line
//!
//! Deliberately out of scope: loading stats from a filesystem or the network,
//! drawing charts, applying colors or emitting ANSI, generating heatmaps, and
//! any clipboard side effect. Everything here is a pure function of its
//! arguments.

use std::collections::HashMap;

/// The date range the stats view covers: all time, 7 days, or 30 days.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatsDateRange {
    /// `all`
    All,
    /// `7d`
    Days7,
    /// `30d`
    Days30,
}

/// The order the date ranges cycle through.
pub const DATE_RANGE_ORDER: [StatsDateRange; 3] = [
    StatsDateRange::All,
    StatsDateRange::Days7,
    StatsDateRange::Days30,
];

/// The label shown for a date range.
pub fn date_range_label(range: StatsDateRange) -> &'static str {
    match range {
        StatsDateRange::Days7 => "Last 7 days",
        StatsDateRange::Days30 => "Last 30 days",
        StatsDateRange::All => "All time",
    }
}

/// The next range in [`DATE_RANGE_ORDER`], wrapping around.
pub fn get_next_date_range(current: StatsDateRange) -> StatsDateRange {
    let index = DATE_RANGE_ORDER
        .iter()
        .position(|range| *range == current)
        .expect("range must be in DATE_RANGE_ORDER");
    DATE_RANGE_ORDER[(index + 1) % DATE_RANGE_ORDER.len()]
}

/// Input fields used when building fun factoids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatsFactoidInput {
    /// Longest session duration in milliseconds.
    pub longest_session_duration_ms: Option<u64>,
}

/// Book token comparison entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookComparison {
    /// Display name.
    pub name: &'static str,
    /// Approximate token count.
    pub tokens: u64,
}

/// Time comparison entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeComparison {
    /// Display name.
    pub name: &'static str,
    /// Approximate minutes.
    pub minutes: u64,
}

/// Canonical book-comparison table used for token factoids.
pub const BOOK_COMPARISONS: [BookComparison; 24] = [
    BookComparison {
        name: "The Little Prince",
        tokens: 22_000,
    },
    BookComparison {
        name: "The Old Man and the Sea",
        tokens: 35_000,
    },
    BookComparison {
        name: "A Christmas Carol",
        tokens: 37_000,
    },
    BookComparison {
        name: "Animal Farm",
        tokens: 39_000,
    },
    BookComparison {
        name: "Fahrenheit 451",
        tokens: 60_000,
    },
    BookComparison {
        name: "The Great Gatsby",
        tokens: 62_000,
    },
    BookComparison {
        name: "Slaughterhouse-Five",
        tokens: 64_000,
    },
    BookComparison {
        name: "Brave New World",
        tokens: 83_000,
    },
    BookComparison {
        name: "The Catcher in the Rye",
        tokens: 95_000,
    },
    BookComparison {
        name: "Harry Potter and the Philosopher's Stone",
        tokens: 103_000,
    },
    BookComparison {
        name: "The Hobbit",
        tokens: 123_000,
    },
    BookComparison {
        name: "1984",
        tokens: 123_000,
    },
    BookComparison {
        name: "To Kill a Mockingbird",
        tokens: 130_000,
    },
    BookComparison {
        name: "Pride and Prejudice",
        tokens: 156_000,
    },
    BookComparison {
        name: "Dune",
        tokens: 244_000,
    },
    BookComparison {
        name: "Moby-Dick",
        tokens: 268_000,
    },
    BookComparison {
        name: "Crime and Punishment",
        tokens: 274_000,
    },
    BookComparison {
        name: "A Game of Thrones",
        tokens: 381_000,
    },
    BookComparison {
        name: "Anna Karenina",
        tokens: 468_000,
    },
    BookComparison {
        name: "Don Quixote",
        tokens: 520_000,
    },
    BookComparison {
        name: "The Lord of the Rings",
        tokens: 576_000,
    },
    BookComparison {
        name: "The Count of Monte Cristo",
        tokens: 603_000,
    },
    BookComparison {
        name: "Les Mis\u{00E9}rables",
        tokens: 689_000,
    },
    BookComparison {
        name: "War and Peace",
        tokens: 730_000,
    },
];

/// Canonical time-comparison table used for duration factoids.
pub const TIME_COMPARISONS: [TimeComparison; 10] = [
    TimeComparison {
        name: "a TED talk",
        minutes: 18,
    },
    TimeComparison {
        name: "an episode of The Office",
        minutes: 22,
    },
    TimeComparison {
        name: "listening to Abbey Road",
        minutes: 47,
    },
    TimeComparison {
        name: "a yoga class",
        minutes: 60,
    },
    TimeComparison {
        name: "a World Cup soccer match",
        minutes: 90,
    },
    TimeComparison {
        name: "a half marathon (average time)",
        minutes: 120,
    },
    TimeComparison {
        name: "the movie Inception",
        minutes: 148,
    },
    TimeComparison {
        name: "watching Titanic",
        minutes: 195,
    },
    TimeComparison {
        name: "a transatlantic flight",
        minutes: 420,
    },
    TimeComparison {
        name: "a full night of sleep",
        minutes: 480,
    },
];

/// Turn a `YYYY-MM-DD` date into `Mon D`. `None` when the date is
/// malformed or the month is out of range.
pub fn format_peak_day(date_str: &str) -> Option<String> {
    let mut parts = date_str.split('-');
    let _year = parts.next()?;
    let month = parts.next()?.parse::<usize>().ok()?;
    let day = parts.next()?.parse::<usize>().ok()?;
    let month_name = match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => return None,
    };
    Some(format!("{month_name} {day}"))
}

/// Collect every factoid that could be chosen from.
pub fn collect_fun_factoids(input: &StatsFactoidInput, total_tokens: u64) -> Vec<String> {
    let mut factoids = Vec::new();
    if total_tokens > 0 {
        for book in BOOK_COMPARISONS
            .iter()
            .filter(|book| total_tokens >= book.tokens)
        {
            let times = total_tokens as f64 / book.tokens as f64;
            if times >= 2.0 {
                factoids.push(format!(
                    "You've used ~{}x more tokens than {}",
                    times.floor() as u64,
                    book.name
                ));
            } else {
                factoids.push(format!(
                    "You've used the same number of tokens as {}",
                    book.name
                ));
            }
        }
    }
    if let Some(duration_ms) = input.longest_session_duration_ms {
        let session_minutes = duration_ms as f64 / (1000.0 * 60.0);
        for comparison in TIME_COMPARISONS {
            let ratio = session_minutes / comparison.minutes as f64;
            if ratio >= 2.0 {
                factoids.push(format!(
                    "Your longest session is ~{}x longer than {}",
                    ratio.floor() as u64,
                    comparison.name
                ));
            }
        }
    }
    factoids
}

/// Deterministic seam for the final selection of one factoid.
pub fn choose_fun_factoid(factoids: &[String], pick_index: usize) -> String {
    if factoids.is_empty() {
        String::new()
    } else {
        factoids[pick_index % factoids.len()].clone()
    }
}

/// Convenience wrapper with injected deterministic selection.
pub fn generate_fun_factoid(
    input: &StatsFactoidInput,
    total_tokens: u64,
    pick_index: usize,
) -> String {
    let factoids = collect_fun_factoids(input, total_tokens);
    choose_fun_factoid(&factoids, pick_index)
}

/// Summary bucket from the shot-distribution view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotBucket {
    /// Display label.
    pub label: &'static str,
    /// Session count in the bucket.
    pub count: u64,
    /// Rounded percentage.
    pub pct: u64,
}

/// Shot-stats summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShotStatsData {
    /// Average shots per session, formatted with one decimal place.
    pub avg_shots: String,
    /// Four fixed buckets, in the order 1, 2–5, 6–10, 11+.
    pub buckets: Vec<ShotBucket>,
}

/// Build the shot summary: four count buckets plus the average shots per
/// session. `None` when the distribution is empty.
pub fn compute_shot_stats(distribution: &HashMap<u64, u64>) -> Option<ShotStatsData> {
    let total = distribution.values().sum::<u64>();
    if total == 0 {
        return None;
    }
    let total_shots = distribution
        .iter()
        .map(|(count, sessions)| count * sessions)
        .sum::<u64>();
    let bucket = |min: u64, max: Option<u64>| -> u64 {
        distribution
            .iter()
            .filter(|(count, _)| {
                **count >= min && max.map(|limit| **count <= limit).unwrap_or(true)
            })
            .map(|(_, sessions)| *sessions)
            .sum()
    };
    let pct = |count: u64| -> u64 { ((count as f64 / total as f64) * 100.0).round() as u64 };
    let b1 = bucket(1, Some(1));
    let b2_5 = bucket(2, Some(5));
    let b6_10 = bucket(6, Some(10));
    let b11 = bucket(11, None);
    Some(ShotStatsData {
        avg_shots: format!("{:.1}", total_shots as f64 / total as f64),
        buckets: vec![
            ShotBucket {
                label: "1-shot",
                count: b1,
                pct: pct(b1),
            },
            ShotBucket {
                label: "2\u{2013}5 shot",
                count: b2_5,
                pct: pct(b2_5),
            },
            ShotBucket {
                label: "6\u{2013}10 shot",
                count: b6_10,
                pct: pct(b6_10),
            },
            ShotBucket {
                label: "11+ shot",
                count: b11,
                pct: pct(b11),
            },
        ],
    })
}

/// One day's token counts, split by model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyModelTokens {
    /// Date in `YYYY-MM-DD`.
    pub date: String,
    /// Total tokens per model.
    pub tokens_by_model: HashMap<String, u64>,
}

/// One rendered series before charting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartSeries {
    /// Raw model id.
    pub model: String,
    /// Caller-rendered model label.
    pub display_name: String,
    /// Per-column token values.
    pub values: Vec<u64>,
}

/// Legend row metadata. Actual ANSI bullet coloring stays deferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartLegendEntry {
    /// Display name.
    pub model: String,
    /// Stable legend index for the color slot.
    pub color_slot: usize,
}

/// Chart-preparation output; drawing the chart is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenChartPlan {
    /// Fixed y-axis width in columns.
    pub y_axis_width: usize,
    /// Derived chart width.
    pub chart_width: usize,
    /// Expanded / sliced data fed into chart series.
    pub recent_data: Vec<DailyModelTokens>,
    /// Visible non-empty series (top 3 models only).
    pub series: Vec<ChartSeries>,
    /// Legend metadata for those series.
    pub legend: Vec<ChartLegendEntry>,
    /// Preformatted x-axis label line.
    pub x_axis_labels: String,
}

/// Prepare the chart data: y-axis width, visible window, the top-3 series,
/// the legend, and the x-axis label line. `None` when there are fewer than
/// two days or no models.
pub fn prepare_token_chart_plan<F>(
    daily_tokens: &[DailyModelTokens],
    models: &[String],
    terminal_width: usize,
    render_model_name: F,
) -> Option<TokenChartPlan>
where
    F: Fn(&str) -> String,
{
    if daily_tokens.len() < 2 || models.is_empty() {
        return None;
    }

    let y_axis_width = 7usize;
    let available_width = terminal_width as i64 - y_axis_width as i64;
    // Clamp into [20, 52] so a degenerate terminal width cannot produce a
    // zero-width chart.
    let chart_width = 52.min(20.max(available_width.max(0) as usize));

    // Too little data to fill the window is repeated until it does; enough
    // data is sliced to the most recent `chart_width` days.
    let recent_data = if daily_tokens.len() >= chart_width {
        daily_tokens[daily_tokens.len() - chart_width..].to_vec()
    } else {
        let repeat_count = chart_width / daily_tokens.len();
        let mut data = Vec::new();
        for day in daily_tokens {
            for _ in 0..repeat_count {
                data.push(day.clone());
            }
        }
        data
    };

    let mut series = Vec::new();
    let mut legend = Vec::new();
    for (index, model) in models.iter().take(3).enumerate() {
        let values = recent_data
            .iter()
            .map(|day| *day.tokens_by_model.get(model).unwrap_or(&0))
            .collect::<Vec<_>>();
        if values.iter().any(|value| *value > 0) {
            let display_name = render_model_name(model);
            series.push(ChartSeries {
                model: model.clone(),
                display_name: display_name.clone(),
                values,
            });
            legend.push(ChartLegendEntry {
                model: display_name,
                color_slot: index,
            });
        }
    }

    if series.is_empty() {
        return None;
    }

    let x_axis_labels = generate_x_axis_labels(&recent_data, y_axis_width);

    Some(TokenChartPlan {
        y_axis_width,
        chart_width,
        recent_data,
        series,
        legend,
        x_axis_labels,
    })
}

/// Build the preformatted x-axis label line for `data`, offset by the
/// y-axis gutter.
pub fn generate_x_axis_labels(data: &[DailyModelTokens], y_axis_offset: usize) -> String {
    if data.is_empty() {
        return String::new();
    }

    // 2 labels for a short window, up to 4 once there is room; the trailing
    // 6 columns are withheld so the last label cannot overhang the chart.
    let num_labels = 4.min(2.max(data.len() / 8));
    let usable_length = data.len().saturating_sub(6);
    let step = (usable_length / (num_labels.saturating_sub(1).max(1))).max(1);

    let mut result = " ".repeat(y_axis_offset);
    let mut current_pos = 0usize;

    for index in 0..num_labels {
        let idx = (index * step).min(data.len() - 1);
        let label = format_peak_day(&data[idx].date).unwrap_or_else(|| data[idx].date.clone());
        let spaces = (idx.saturating_sub(current_pos)).max(1);
        result.push_str(&" ".repeat(spaces));
        result.push_str(&label);
        current_pos = idx + label.len();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(date: &str, entries: &[(&str, u64)]) -> DailyModelTokens {
        DailyModelTokens {
            date: date.into(),
            tokens_by_model: entries
                .iter()
                .map(|(model, tokens)| ((*model).into(), *tokens))
                .collect(),
        }
    }

    #[test]
    fn date_range_cycles_through_all_ranges() {
        assert_eq!(
            get_next_date_range(StatsDateRange::All),
            StatsDateRange::Days7
        );
        assert_eq!(
            get_next_date_range(StatsDateRange::Days7),
            StatsDateRange::Days30
        );
        assert_eq!(
            get_next_date_range(StatsDateRange::Days30),
            StatsDateRange::All
        );
        assert_eq!(date_range_label(StatsDateRange::Days30), "Last 30 days");
    }

    #[test]
    fn format_peak_day_matches_en_us_short_month() {
        assert_eq!(format_peak_day("2026-04-09").as_deref(), Some("Apr 9"));
        assert_eq!(format_peak_day("2026-12-01").as_deref(), Some("Dec 1"));
        assert_eq!(format_peak_day("bad"), None);
    }

    #[test]
    fn collect_fun_factoids_builds_book_and_time_rows() {
        let factoids = collect_fun_factoids(
            &StatsFactoidInput {
                longest_session_duration_ms: Some(4 * 60 * 60 * 1000),
            },
            80_000,
        );
        assert!(factoids.iter().any(|fact| fact.contains("Fahrenheit 451")));
        assert!(factoids
            .iter()
            .any(|fact| fact.contains("longer than a TED talk")));
    }

    #[test]
    fn generate_fun_factoid_uses_injected_pick_index() {
        let input = StatsFactoidInput {
            longest_session_duration_ms: Some(4 * 60 * 60 * 1000),
        };
        let factoids = collect_fun_factoids(&input, 80_000);
        let picked = generate_fun_factoid(&input, 80_000, 1);
        assert_eq!(picked, factoids[1]);
        assert_eq!(
            generate_fun_factoid(&StatsFactoidInput::default(), 0, 0),
            ""
        );
    }

    #[test]
    fn shot_stats_match_bucket_math() {
        let distribution = HashMap::from([(1, 2), (3, 3), (7, 4), (12, 1)]);
        let summary = compute_shot_stats(&distribution).expect("summary");
        assert_eq!(summary.avg_shots, "5.1");
        assert_eq!(summary.buckets[0].count, 2);
        assert_eq!(summary.buckets[1].count, 3);
        assert_eq!(summary.buckets[2].count, 4);
        assert_eq!(summary.buckets[3].count, 1);
        assert_eq!(summary.buckets[2].pct, 40);
    }

    #[test]
    fn shot_stats_returns_none_for_empty_total() {
        assert!(compute_shot_stats(&HashMap::new()).is_none());
    }

    #[test]
    fn chart_plan_slices_recent_data_when_longer_than_width() {
        let days = (1..=30)
            .map(|day_num| day(&format!("2026-01-{day_num:02}"), &[("opus", day_num)]))
            .collect::<Vec<_>>();
        let plan = prepare_token_chart_plan(&days, &[String::from("opus")], 30, |name| {
            name.to_uppercase()
        })
        .expect("plan");
        assert_eq!(plan.chart_width, 23);
        assert_eq!(plan.recent_data.len(), 23);
        assert_eq!(
            plan.recent_data.first().map(|day| day.date.as_str()),
            Some("2026-01-08")
        );
        assert_eq!(plan.legend[0].model, "OPUS");
    }

    #[test]
    fn chart_plan_repeats_data_and_skips_empty_series() {
        let days = vec![
            day("2026-01-01", &[("opus", 10), ("haiku", 0)]),
            day("2026-01-02", &[("opus", 20), ("haiku", 0)]),
        ];
        let plan = prepare_token_chart_plan(
            &days,
            &[
                String::from("opus"),
                String::from("haiku"),
                String::from("sonnet"),
            ],
            40,
            |name| name.into(),
        )
        .expect("plan");
        assert_eq!(plan.chart_width, 33);
        assert_eq!(plan.recent_data.len(), 32);
        assert_eq!(plan.series.len(), 1);
        assert_eq!(plan.series[0].values.len(), 32);
    }

    #[test]
    fn chart_plan_requires_two_days_and_non_empty_models() {
        assert!(
            prepare_token_chart_plan(&[], &[String::from("opus")], 80, |name| name.into())
                .is_none()
        );
        assert!(prepare_token_chart_plan(
            &[day("2026-01-01", &[("opus", 1)])],
            &[String::from("opus")],
            80,
            |name| name.into()
        )
        .is_none());
        assert!(prepare_token_chart_plan(
            &[
                day("2026-01-01", &[("opus", 1)]),
                day("2026-01-02", &[("opus", 2)])
            ],
            &[],
            80,
            |name| name.into()
        )
        .is_none());
    }

    #[test]
    fn x_axis_labels_include_offset_and_spaced_dates() {
        let days = (1..=20)
            .map(|day_num| day(&format!("2026-02-{day_num:02}"), &[("opus", day_num)]))
            .collect::<Vec<_>>();
        let labels = generate_x_axis_labels(&days, 7);
        assert!(labels.starts_with("       "));
        assert!(labels.contains("Feb 1"));
        assert!(
            labels.contains("Feb 15") || labels.contains("Feb 16") || labels.contains("Feb 20")
        );
    }
}
