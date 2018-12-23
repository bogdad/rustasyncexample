use std::ops;

#[derive(Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct MioReady(usize);
const READABLE: usize = 0b00001;
const WRITABLE: usize = 0b00010;
const ERROR: usize = 0b00100;
const HUP: usize = 0b01000;
// macos ios freebsd
const AIO: usize = 0b01_0000;
// not freebsd
const LIO: usize = 0b00_0000;
// sys
pub const READY_ALL: usize = ERROR | HUP | AIO | LIO;
impl MioReady {
    pub fn empty() -> MioReady {
        MioReady(0)
    }
    pub fn none() -> MioReady {
        MioReady::empty()
    }
    #[inline]
    pub fn readable() -> MioReady {
        MioReady(READABLE)
    }
    #[inline]
    pub fn writable() -> MioReady {
        MioReady(WRITABLE)
    }
    #[inline]
    pub fn error() -> MioReady {
        MioReady(ERROR)
    }
    #[inline]
    pub fn hup() -> MioReady {
        MioReady(HUP)
    }
    #[inline]
    pub fn all() -> MioReady {
        MioReady(READABLE | WRITABLE | READY_ALL)
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        *self == MioReady::empty()
    }
    #[inline]
    pub fn is_none(&self) -> bool {
        self.is_empty()
    }
    #[inline]
    pub fn is_readable(&self) -> bool {
        self.contains(MioReady::readable())
    }
    #[inline]
    pub fn is_writable(&self) -> bool {
        self.contains(MioReady::writable())
    }
    #[inline]
    pub fn is_error(&self) -> bool {
        self.contains(MioReady(ERROR))
    }
    #[inline]
    pub fn is_hup(&self) -> bool {
        self.contains(MioReady(HUP))
    }
    #[inline]
    pub fn insert<T: Into<Self>>(&mut self, other: T) {
        let other = other.into();
        self.0 |= other.0;
    }
    #[inline]
    pub fn remove<T: Into<Self>>(&mut self, other: T) {
        let other = other.into();
        self.0 &= !other.0;
    }
    #[inline]
    pub fn bits(&self) -> usize {
        self.0
    }
    #[inline]
    pub fn contains<T: Into<Self>>(&self, other: T) -> bool {
        let other = other.into();
        (*self & other) == other
    }
}

impl<T: Into<MioReady>> ops::BitAnd<T> for MioReady {
    type Output = MioReady;

    #[inline]
    fn bitand(self, other: T) -> MioReady {
        MioReady(self.0 & other.into().0)
    }
}

#[derive(Copy, PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct MioPollOpt(usize);

impl MioPollOpt {
    #[inline]
    pub fn empty() -> MioPollOpt {
        MioPollOpt(0)
    }
    #[inline]
    pub fn edge() -> MioPollOpt {
        MioPollOpt(0b0001)
    }
    #[inline]
    pub fn oneshot() -> MioPollOpt {
        MioPollOpt(0b0100)
    }
    #[inline]
    pub fn contains(&self, other: MioPollOpt) -> bool {
        (*self & other) == other
    }
}

impl ops::BitAnd for MioPollOpt {
    type Output = MioPollOpt;

    #[inline]
    fn bitand(self, other: MioPollOpt) -> MioPollOpt {
        MioPollOpt(self.0 & other.0)
    }
}
