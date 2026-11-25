/// Whether `year` is a leap year in the Gregorian calendar.
pub fn is_leap_year(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}
