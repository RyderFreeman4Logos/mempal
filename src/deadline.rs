#[derive(Debug)]
pub(crate) struct Exceeded;

impl std::fmt::Display for Exceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "CLI search total deadline exceeded after 120s")
    }
}

impl std::error::Error for Exceeded {}

pub(crate) fn is(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|source| source.downcast_ref::<Exceeded>().is_some())
}
