use crate::decl::parse_pools;
use crate::needs::{Need, NeedMode, parse_need};

fn need(text: &str) -> Result<Need, String> {
    let table: toml::Table = toml::from_str(&format!("v = {text}")).unwrap();
    parse_need(&table["v"])
}

const POOLS: &str = r#"schema = 1
[[pool]]
name = "jobs"
kind = "slots"
size = 2
[[pool]]
name = "db"
kind = "rw"
[[pool]]
name = "lock"
kind = "mutex"
[[pool]]
name = "mem"
kind = "guard"
guard = { free_memory_gb = 8 }
"#;

fn check(need_text: &str, pool: usize) -> Result<(), String> {
    let pools = parse_pools(POOLS).unwrap().decls;
    need(need_text)?.check_against(&pools[pool])
}

#[test]
fn a_name_and_a_table_parse() {
    assert_eq!(
        need("\"jobs\"").unwrap(),
        Need {
            pool: "jobs".to_string(),
            mode: None,
            count: None
        }
    );
    assert_eq!(
        need("{ pool = \"db\", mode = \"exclusive\" }")
            .unwrap()
            .mode,
        Some(NeedMode::Exclusive)
    );
    assert_eq!(need("{ pool = \"jobs\", count = 2 }").unwrap().count(), 2);
    assert_eq!(need("\"jobs\"").unwrap().count(), 1);
}

#[test]
fn bad_entries_are_refused() {
    assert!(need("3").is_err());
    assert!(need("{ mode = \"shared\" }").is_err());
    assert!(need("{ pool = \"a\", mode = \"both\" }").is_err());
    assert!(need("{ pool = \"a\", count = 0 }").is_err());
    assert!(need("{ pool = \"a\", extra = 1 }").is_err());
}

#[test]
fn mode_on_a_slots_pool_is_refused() {
    let error = check("{ pool = \"jobs\", mode = \"exclusive\" }", 0).unwrap_err();
    assert!(error.contains("only valid on an rw pool"), "{error}");
    assert!(check("{ pool = \"db\", mode = \"exclusive\" }", 1).is_ok());
    assert!(check("\"db\"", 1).is_ok());
}

#[test]
fn count_above_size_is_refused() {
    assert!(check("{ pool = \"jobs\", count = 2 }", 0).is_ok());
    let error = check("{ pool = \"jobs\", count = 3 }", 0).unwrap_err();
    assert!(error.contains("above the size 2"), "{error}");
    assert!(check("{ pool = \"lock\", count = 2 }", 2).is_err());
    assert!(check("{ pool = \"lock\", count = 1 }", 2).is_ok());
    assert!(check("{ pool = \"db\", count = 1 }", 1).is_err());
    assert!(check("{ pool = \"mem\", count = 1 }", 3).is_err());
    assert!(check("{ pool = \"mem\", mode = \"shared\" }", 3).is_err());
    assert!(check("\"mem\"", 3).is_ok());
}
