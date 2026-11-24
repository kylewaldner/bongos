use super::*;

#[test]
fn a_term_in_the_file_name_outranks_the_same_term_in_a_directory() {
    let in_name = score("src/other/Buffer.kt", &["buffer"]).unwrap();
    let in_dir = score("src/buffer/Other.kt", &["buffer"]).unwrap();
    assert!(in_name > in_dir);
    let ranked = rank(
        &["src/buffer/Other.kt", "src/other/Buffer.kt"],
        &["buffer"],
        10,
    );
    assert_eq!(ranked, vec!["src/other/Buffer.kt", "src/buffer/Other.kt"]);
}

#[test]
fn a_slash_term_matching_contiguous_segments_outranks_scattered_characters() {
    let contiguous = score("app/stuff/File.kt", &["stuff/File"]).unwrap();
    let scattered = score("app/stuffing/more/File.kt", &["stuff/File"]).unwrap();
    assert!(contiguous > scattered);
    assert_eq!(score("app/other.kt", &["stuff/File"]), None);
}

#[test]
fn equal_scores_keep_input_order() {
    let ranked = rank(&["b/x.kt", "a/x.kt", "c/x.kt"], &["x"], 2);
    assert_eq!(ranked, vec!["b/x.kt", "a/x.kt"]);
}

mod perf_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn synthetic_paths(count: usize) -> Vec<String> {
        let dirs = [
            "core", "ui", "model", "net", "editor", "util", "stuff", "app",
        ];
        let names = ["Buffer", "Window", "Picker", "Router", "Index", "Cache"];
        (0..count)
            .map(|i| {
                format!(
                    "{}/{}/{}{}/{}{}.kt",
                    dirs[i % 8],
                    dirs[(i / 8) % 8],
                    dirs[(i / 64) % 8],
                    i % 97,
                    names[(i / 7) % 6],
                    i
                )
            })
            .collect()
    }

    #[test]
    fn re_ranking_2000_of_100000_paths_stays_under_8_ms() {
        let all = synthetic_paths(100_000);
        let candidates: Vec<&str> = all.iter().step_by(50).map(String::as_str).collect();
        assert_eq!(candidates.len(), 2000);
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let start = Instant::now();
            let ranked = rank(&candidates, &["buf", "stuff/"], 50);
            best = best.min(start.elapsed());
            assert!(!ranked.is_empty());
        }
        assert!(
            best < Duration::from_millis(8),
            "best of three was {best:?}"
        );
    }
}
