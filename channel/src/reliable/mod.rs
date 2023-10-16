pub mod tcp;

pub trait NonblockableIO {
    fn is_nonblocking(&self) -> bool;
}
