// str_util.rs

pub trait ConvertWhiteSpace {
    fn ws_convert(self) -> String;
}
impl<S> ConvertWhiteSpace for S
where
    S: AsRef<str>,
{
    fn ws_convert(self) -> String {
        self.as_ref()
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join("_")
    }
}
// EOF
