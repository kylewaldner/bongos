use crate::rectangle::{Cell, Columns, Locate, cells, line_width, locate};

fn cell(byte: usize, start_col: usize, width: usize) -> Cell {
    Cell {
        byte,
        start_col,
        width,
    }
}

#[test]
fn ascii_columns() {
    assert_eq!(
        cells("abc", 8),
        vec![cell(0, 0, 1), cell(1, 1, 1), cell(2, 2, 1)]
    );
    assert_eq!(line_width("abc", 8), 3);
    assert_eq!(line_width("", 8), 0);
    assert_eq!(locate("abc", 1, 8), Locate::At(1));
    assert_eq!(locate("abc", 3, 8), Locate::At(3));
    let columns = Columns::from_corners(5, 2);
    assert_eq!(columns, Columns { left: 2, right: 5 });
    assert_eq!(columns.width(), 3);
    assert!(!columns.is_empty());
    assert!(Columns::from_corners(4, 4).is_empty());
}

#[test]
fn tab_widths_depend_on_offset_and_size() {
    assert_eq!(cells("\tx", 8), vec![cell(0, 0, 8), cell(1, 8, 1)]);
    assert_eq!(cells("a\tx", 8)[1].width, 7);
    assert_eq!(cells("a\tx", 4)[1].width, 3);
    assert_eq!(cells("a\tx", 2)[1].width, 1);
    assert_eq!(cells("ab\tx", 2)[2].width, 2);
    assert_eq!(line_width("abc\t", 4), 4);
    assert_eq!(line_width("abcd\t", 4), 8);
}

#[test]
fn wide_and_combining_characters() {
    let line = "a\u{4e2d}\u{1f600}e\u{301}";
    let all = cells(line, 4);
    assert_eq!(all[1], cell(1, 1, 2));
    assert_eq!(all[2], cell(4, 3, 2));
    assert_eq!(all[3], cell(8, 5, 1));
    assert_eq!(all[4], cell(9, 6, 0));
    assert_eq!(line_width(line, 4), 6);
}

#[test]
fn locate_inside_a_tab() {
    assert_eq!(
        locate("a\tx", 3, 8),
        Locate::InsideTab {
            byte: 1,
            before: 2,
            after: 5
        }
    );
    assert_eq!(locate("a\tx", 8, 8), Locate::At(2));
}

#[test]
fn locate_inside_a_wide_character() {
    assert_eq!(locate("a\u{4e2d}b", 2, 4), Locate::InsideWide { byte: 1 });
    assert_eq!(locate("a\u{4e2d}b", 3, 4), Locate::At(4));
}

#[test]
fn locate_past_the_end() {
    assert_eq!(locate("abc", 5, 8), Locate::PastEnd { short_by: 2 });
    assert_eq!(locate("", 1, 8), Locate::PastEnd { short_by: 1 });
    assert_eq!(locate("abc", 3, 8), Locate::At(3));
}
