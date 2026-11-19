//! Pure evaluation logic: the matching rule, per-query judgements, rank
//! metrics and nearest-rank percentiles. Nothing in this module performs I/O.

use serde::Serialize;

/// Number of leading results Recall@5 looks at.
pub const RECALL_CUTOFF: usize = 5;
/// Number of leading results MRR@10 looks at.
pub const MRR_CUTOFF: usize = 10;

/// Inclusive, 1-based line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

/// Where an annotated query expects its answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Expectation {
    pub path: String,
    pub symbol: String,
    pub lines: Option<LineRange>,
}

/// The location one search result points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    pub path: String,
    pub symbol: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

/// A result symbol names the expected one when it equals it, or is a qualified
/// name ending in `.symbol` or `::symbol`.
pub fn symbol_matches(found: Option<&str>, expected: &str) -> bool {
    found.is_some_and(|found| {
        found == expected
            || found
                .strip_suffix(expected)
                .is_some_and(|scope| scope.ends_with('.') || scope.ends_with("::"))
    })
}

/// The matching rule: same project-relative path and a matching symbol. Line
/// numbers are deliberately not part of it; see [`TopOutcome::WrongLines`].
pub fn location_matches(found: &Location, expected: &Expectation) -> bool {
    found.path == expected.path && symbol_matches(found.symbol.as_deref(), &expected.symbol)
}

/// What the first result of a query points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TopOutcome {
    /// The expected location, including the annotated line range if there is one.
    Expected,
    /// The expected path and symbol, but not the annotated line range.
    WrongLines,
    /// Some other path or symbol.
    WrongLocation,
    /// The search returned nothing.
    NoResults,
}

impl TopOutcome {
    /// Both kinds of wrong answer count towards the wrong-location rate.
    pub fn is_wrong_location(self) -> bool {
        matches!(self, Self::WrongLines | Self::WrongLocation)
    }
}

/// How one result list compares with its annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Judgement {
    /// 1-based rank of the first result matching the expected path and symbol.
    pub rank: Option<usize>,
    pub top: TopOutcome,
}

pub fn judge(expected: &Expectation, results: &[Location]) -> Judgement {
    let rank = results
        .iter()
        .position(|found| location_matches(found, expected))
        .map(|index| index + 1);
    let top = match results.first() {
        None => TopOutcome::NoResults,
        Some(first) if !location_matches(first, expected) => TopOutcome::WrongLocation,
        Some(first) => match expected.lines {
            Some(lines) if (first.start_line, first.end_line) != (lines.start, lines.end) => {
                TopOutcome::WrongLines
            }
            _ => TopOutcome::Expected,
        },
    };
    Judgement { rank, top }
}

/// Rank metrics over a group of queries. Every rate is a fraction of `queries`,
/// which includes the queries whose search failed (`errors`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankMetrics {
    pub queries: usize,
    /// Queries whose search returned an error; they score zero everywhere and
    /// are counted neither as "wrong location" nor as "no result".
    pub errors: usize,
    /// Fraction with the expected path and symbol at rank 1.
    pub top1: f64,
    /// Fraction with the expected path and symbol among the first five results.
    pub recall_at_5: f64,
    /// Mean of `1 / rank`, taking 0 when the rank is missing or above 10.
    pub mrr_at_10: f64,
    /// Fraction that returned results whose first result is not the expected
    /// location (other path or symbol, or other lines than annotated).
    pub wrong_location_rate: f64,
    /// Fraction that returned no result at all.
    pub no_result_rate: f64,
    pub hits_at_1: usize,
    pub hits_at_5: usize,
    pub wrong_location: usize,
    /// The part of `wrong_location` where only the line range differed.
    pub wrong_lines: usize,
    pub no_result: usize,
}

/// Aggregates judgements; `None` stands for a query whose search failed. An
/// empty group yields zero rates.
pub fn rank_metrics<I>(judgements: I) -> RankMetrics
where
    I: IntoIterator<Item = Option<Judgement>>,
{
    let (mut queries, mut errors) = (0usize, 0usize);
    let (mut hits_at_1, mut hits_at_5, mut reciprocal) = (0usize, 0usize, 0.0f64);
    let (mut wrong_location, mut wrong_lines, mut no_result) = (0usize, 0usize, 0usize);
    for judgement in judgements {
        queries += 1;
        let Some(judgement) = judgement else {
            errors += 1;
            continue;
        };
        if let Some(rank) = judgement.rank {
            hits_at_1 += usize::from(rank == 1);
            hits_at_5 += usize::from(rank <= RECALL_CUTOFF);
            if rank <= MRR_CUTOFF {
                reciprocal += 1.0 / rank as f64;
            }
        }
        wrong_location += usize::from(judgement.top.is_wrong_location());
        wrong_lines += usize::from(judgement.top == TopOutcome::WrongLines);
        no_result += usize::from(judgement.top == TopOutcome::NoResults);
    }
    let rate = |count: usize| {
        if queries == 0 {
            0.0
        } else {
            count as f64 / queries as f64
        }
    };
    RankMetrics {
        queries,
        errors,
        top1: rate(hits_at_1),
        recall_at_5: rate(hits_at_5),
        mrr_at_10: if queries == 0 {
            0.0
        } else {
            reciprocal / queries as f64
        },
        wrong_location_rate: rate(wrong_location),
        no_result_rate: rate(no_result),
        hits_at_1,
        hits_at_5,
        wrong_location,
        wrong_lines,
        no_result,
    }
}

/// Nearest-rank percentile of ascending-sorted samples: the sample at 1-based
/// rank `ceil(percent / 100 * n)`. There is no interpolation, so the result is
/// always an observed sample. `percent` must be in `1..=100`.
pub fn percentile(sorted: &[f64], percent: usize) -> Option<f64> {
    debug_assert!((1..=100).contains(&percent), "percent must be in 1..=100");
    debug_assert!(
        sorted.windows(2).all(|pair| pair[0] <= pair[1]),
        "samples must be sorted ascending"
    );
    // Integer arithmetic: `0.95 * n` is not exact in floating point.
    let rank = sorted
        .len()
        .checked_mul(percent.clamp(1, 100))?
        .div_ceil(100);
    sorted.get(rank.checked_sub(1)?).copied()
}

/// Smallest sample count for which the nearest-rank percentile is not simply
/// the largest sample: `ceil(100 / (100 - percent))`, i.e. 2 for P50 and 20 for
/// P95. `None` for the 100th percentile, which is the maximum by definition.
pub fn percentile_min_samples(percent: usize) -> Option<usize> {
    (percent < 100).then(|| 100usize.div_ceil(100 - percent))
}

fn undersampled(samples: usize, percent: usize) -> bool {
    percentile_min_samples(percent).is_none_or(|minimum| samples < minimum)
}

/// Milliseconds rounded to whole microseconds.
pub fn round_ms(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

/// Distribution of wall-clock samples, in milliseconds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimingSummary {
    pub samples: usize,
    pub mean_ms: Option<f64>,
    pub min_ms: Option<f64>,
    /// Nearest-rank median.
    pub p50_ms: Option<f64>,
    /// Nearest-rank 95th percentile.
    pub p95_ms: Option<f64>,
    pub max_ms: Option<f64>,
    /// Fewer than 2 samples: the reported P50 is the only sample, or absent.
    pub p50_undersampled: bool,
    /// Fewer than 20 samples: the reported P95 is the maximum, or absent.
    pub p95_undersampled: bool,
}

pub fn summarize(samples_ms: &[f64]) -> TimingSummary {
    let mut sorted = samples_ms.to_vec();
    sorted.sort_by(f64::total_cmp);
    let count = sorted.len();
    TimingSummary {
        samples: count,
        mean_ms: (count > 0).then(|| round_ms(sorted.iter().sum::<f64>() / count as f64)),
        min_ms: sorted.first().copied(),
        p50_ms: percentile(&sorted, 50),
        p95_ms: percentile(&sorted, 95),
        max_ms: sorted.last().copied(),
        p50_undersampled: undersampled(count, 50),
        p95_undersampled: undersampled(count, 95),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(path: &str, symbol: Option<&str>, start_line: usize, end_line: usize) -> Location {
        Location {
            path: path.into(),
            symbol: symbol.map(str::to_owned),
            start_line,
            end_line,
        }
    }

    fn expectation(path: &str, symbol: &str, lines: Option<(usize, usize)>) -> Expectation {
        Expectation {
            path: path.into(),
            symbol: symbol.into(),
            lines: lines.map(|(start, end)| LineRange { start, end }),
        }
    }

    #[test]
    fn symbols_match_exactly_or_as_a_qualified_suffix() {
        for found in [
            "find_user",
            "UserRepository.find_user",
            "repo::UserRepository::find_user",
            "a.b::find_user",
        ] {
            assert!(symbol_matches(Some(found), "find_user"), "{found}");
        }
        for found in [
            "refind_user",
            "find_user_by_id",
            "UserRepository.find_user.inner",
            "UserRepository:find_user",
            "Find_user",
            "",
        ] {
            assert!(!symbol_matches(Some(found), "find_user"), "{found}");
        }
        assert!(!symbol_matches(None, "find_user"));
        assert!(symbol_matches(Some("crate::a::b"), "a::b"));
    }

    #[test]
    fn matching_requires_the_same_path_and_ignores_lines() {
        let expected = expectation("src/users.rs", "login", Some((6, 12)));
        assert!(location_matches(
            &location("src/users.rs", Some("Auth::login"), 40, 50),
            &expected
        ));
        assert!(!location_matches(
            &location("users.rs", Some("login"), 6, 12),
            &expected
        ));
        assert!(!location_matches(
            &location("src/users.rs", None, 6, 12),
            &expected
        ));
    }

    #[test]
    fn judgement_separates_rank_from_what_the_top_result_points_at() {
        let expected = expectation("a.rs", "target", Some((6, 12)));
        let other = location("b.rs", Some("other"), 1, 2);
        let exact = location("a.rs", Some("target"), 6, 12);
        let moved = location("a.rs", Some("target"), 7, 13);

        assert_eq!(
            judge(&expected, &[]),
            Judgement {
                rank: None,
                top: TopOutcome::NoResults
            }
        );
        assert_eq!(
            judge(&expected, &[exact.clone(), other.clone()]),
            Judgement {
                rank: Some(1),
                top: TopOutcome::Expected
            }
        );
        assert_eq!(
            judge(&expected, &[other.clone(), other.clone(), exact.clone()]),
            Judgement {
                rank: Some(3),
                top: TopOutcome::WrongLocation
            }
        );
        assert_eq!(
            judge(&expected, std::slice::from_ref(&other)),
            Judgement {
                rank: None,
                top: TopOutcome::WrongLocation
            }
        );
        // Right symbol, other lines: still found at rank 1, but a wrong location.
        assert_eq!(
            judge(&expected, &[moved.clone(), exact]),
            Judgement {
                rank: Some(1),
                top: TopOutcome::WrongLines
            }
        );
        // Without an annotated range any line range is accepted.
        assert_eq!(
            judge(&expectation("a.rs", "target", None), &[moved]),
            Judgement {
                rank: Some(1),
                top: TopOutcome::Expected
            }
        );
    }

    #[test]
    fn rank_metrics_apply_the_cutoffs_and_count_failures_in_the_denominator() {
        let judged = |rank: Option<usize>, top: TopOutcome| Some(Judgement { rank, top });
        let metrics = rank_metrics([
            judged(Some(1), TopOutcome::Expected),
            judged(Some(1), TopOutcome::WrongLines),
            judged(Some(2), TopOutcome::WrongLocation),
            judged(Some(5), TopOutcome::WrongLocation),
            judged(Some(6), TopOutcome::WrongLocation),
            judged(Some(10), TopOutcome::WrongLocation),
            judged(Some(11), TopOutcome::WrongLocation),
            judged(None, TopOutcome::WrongLocation),
            judged(None, TopOutcome::NoResults),
            None,
        ]);
        assert_eq!(metrics.queries, 10);
        assert_eq!(metrics.errors, 1);
        assert_eq!((metrics.hits_at_1, metrics.hits_at_5), (2, 4));
        assert_eq!(metrics.top1, 0.2);
        assert_eq!(metrics.recall_at_5, 0.4);
        let reciprocal = 1.0 + 1.0 + 1.0 / 2.0 + 1.0 / 5.0 + 1.0 / 6.0 + 1.0 / 10.0;
        assert!((metrics.mrr_at_10 - reciprocal / 10.0).abs() < 1e-12);
        assert_eq!((metrics.wrong_location, metrics.wrong_lines), (7, 1));
        assert_eq!(metrics.wrong_location_rate, 0.7);
        assert_eq!(metrics.no_result, 1);
        assert_eq!(metrics.no_result_rate, 0.1);
        // Correct top results, wrong locations, empty results and failures
        // partition the queries.
        let correct_top = metrics.hits_at_1 - metrics.wrong_lines;
        assert_eq!(
            correct_top + metrics.wrong_location + metrics.no_result + metrics.errors,
            metrics.queries
        );
    }

    #[test]
    fn perfect_and_empty_groups() {
        let perfect = rank_metrics(std::iter::repeat_n(
            Some(Judgement {
                rank: Some(1),
                top: TopOutcome::Expected,
            }),
            18,
        ));
        assert_eq!(
            (perfect.top1, perfect.recall_at_5, perfect.mrr_at_10),
            (1.0, 1.0, 1.0)
        );
        assert_eq!(
            (perfect.wrong_location_rate, perfect.no_result_rate),
            (0.0, 0.0)
        );
        let empty = rank_metrics([]);
        assert_eq!(empty.queries, 0);
        assert_eq!((empty.top1, empty.mrr_at_10), (0.0, 0.0));
    }

    #[test]
    fn nearest_rank_percentiles_return_observed_samples() {
        // The standard worked example for the nearest-rank method.
        let samples = [15.0, 20.0, 35.0, 40.0, 50.0];
        for (percent, expected) in [(5, 15.0), (30, 20.0), (40, 20.0), (50, 35.0), (100, 50.0)] {
            assert_eq!(percentile(&samples, percent), Some(expected), "P{percent}");
        }
        assert_eq!(percentile(&[], 95), None);
        assert_eq!(percentile(&[7.0], 50), Some(7.0));
        assert_eq!(percentile(&[7.0], 95), Some(7.0));

        let twenty: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(percentile(&twenty, 95), Some(19.0));
        assert_eq!(percentile(&twenty, 50), Some(10.0));
        let nineteen = &twenty[..19];
        assert_eq!(percentile(nineteen, 95), Some(19.0));
        let thirty: Vec<f64> = (1..=30).map(f64::from).collect();
        assert_eq!(percentile(&thirty, 95), Some(29.0));
        assert_eq!(percentile(&thirty, 50), Some(15.0));
        // Rank 7 exactly; floating point `0.07 * 100.0` would round up to 8.
        let hundred: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&hundred, 7), Some(7.0));
    }

    #[test]
    fn minimum_sample_counts_are_where_the_percentile_leaves_the_maximum() {
        assert_eq!(percentile_min_samples(50), Some(2));
        assert_eq!(percentile_min_samples(95), Some(20));
        assert_eq!(percentile_min_samples(99), Some(100));
        assert_eq!(percentile_min_samples(100), None);
        for percent in [50, 90, 95, 99] {
            let minimum = percentile_min_samples(percent).unwrap();
            let rank = |count: usize| (count * percent).div_ceil(100);
            assert!(rank(minimum) < minimum, "P{percent}");
            assert_eq!(rank(minimum - 1), minimum - 1, "P{percent}");
        }
    }

    #[test]
    fn summaries_flag_percentiles_without_enough_samples() {
        let empty = summarize(&[]);
        assert_eq!(empty.samples, 0);
        assert_eq!(
            (
                empty.mean_ms,
                empty.min_ms,
                empty.p50_ms,
                empty.p95_ms,
                empty.max_ms
            ),
            (None, None, None, None, None)
        );
        assert!(empty.p50_undersampled && empty.p95_undersampled);

        let single = summarize(&[4.0]);
        assert_eq!((single.p50_ms, single.p95_ms), (Some(4.0), Some(4.0)));
        assert!(single.p50_undersampled && single.p95_undersampled);

        let unsorted: Vec<f64> = (1..=19).rev().map(f64::from).collect();
        let nineteen = summarize(&unsorted);
        assert_eq!(nineteen.samples, 19);
        assert_eq!((nineteen.min_ms, nineteen.max_ms), (Some(1.0), Some(19.0)));
        assert_eq!(nineteen.mean_ms, Some(10.0));
        assert_eq!(nineteen.p50_ms, Some(10.0));
        assert_eq!(nineteen.p95_ms, nineteen.max_ms);
        assert!(!nineteen.p50_undersampled);
        assert!(nineteen.p95_undersampled);

        let twenty: Vec<f64> = (1..=20).map(f64::from).collect();
        let twenty = summarize(&twenty);
        assert_eq!(twenty.p95_ms, Some(19.0));
        assert!(!twenty.p95_undersampled);
        assert_eq!(summarize(&[0.1, 0.2, 0.4]).mean_ms, Some(0.233));
    }
}
