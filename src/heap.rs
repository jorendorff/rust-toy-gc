//! GC heap allocator.

use std::cell::RefCell;
use std::collections::HashMap;
use std::default::Default;
use std::marker::PhantomData;
use std::{fmt, mem, ptr};
use bit_vec::BitVec;

// What does this do? You'll never guess!
type HeapId<'a> = PhantomData<::std::cell::Cell<&'a mut ()>>;

/// Total number of Pair objects that can be allocated in a Heap at once.
///
/// XXX BOGUS: At present, this is number is carefully selected (with insider
/// knowledge of both `size_of::<PairStorage>()` on my platform and the `Box`
/// allocator's behavior) so that a HeapStorage object fits in a page and thus
/// the assertions below about HEAP_STORAGE_ALIGN pass.
pub const HEAP_SIZE: usize = 125;

/// We rely on all bits to the right of this bit being 0 in addresses of
/// HeapStorage instances.
const HEAP_STORAGE_ALIGN: usize = 0x1000;

// The heap is (for now) just a big array of Pairs
struct HeapStorage<'a, T: GCThing<'a>> {
    heap: *mut Heap<'a>,
    mark_bits: BitVec,
    allocated_bits: BitVec,
    mark_entry_point: unsafe fn(*mut ()),
    freelist: *mut (),
    objects: [T; HEAP_SIZE]
}

impl<'a, T: GCThing<'a>> Drop for HeapStorage<'a, T> {
    fn drop(&mut self) {
        // All the memory in self.objects is uninitialized at this point. Rust
        // will drop each of those objects, which would crash. So we have this
        // super lame hack: initialize all the objects with innocuous values so
        // they can be safely destroyed.
        for i in 0 .. HEAP_SIZE {
            unsafe {
                ptr::write(
                    &mut self.objects[i] as *mut T,
                    T::default());
            }
        }
    }
}

impl<'a, T: GCThing<'a>> HeapStorage<'a, T> {
    unsafe fn new(heap: *mut Heap<'a>) -> Box<HeapStorage<'a, T>> {
        let mut storage = Box::new(HeapStorage {
            heap: heap,
            mark_bits: BitVec::from_elem(HEAP_SIZE, false),
            allocated_bits: BitVec::from_elem(HEAP_SIZE, false),
            mark_entry_point: mark_entry_point::<T>,
            freelist: ptr::null_mut(),
            objects: mem::uninitialized()
        });

        // These assertions will likely fail on 32-bit platforms or if someone
        // is somehow using a custom Box allocator. If either one fails, this
        // GC will not work.
        assert_eq!(&mut *storage as *mut HeapStorage<'a, T> as usize & (HEAP_STORAGE_ALIGN - 1), 0);
        assert!(mem::size_of::<HeapStorage<'a, T>>() <= HEAP_STORAGE_ALIGN);

        for i in 0 .. HEAP_SIZE {
            let p = &mut storage.objects[i] as *mut T;
            storage.add_to_free_list(p);
        }
        storage
    }

    unsafe fn add_to_free_list(&mut self, p: *mut T) {
        let listp = p as *mut *mut ();
        *listp = self.freelist;
        assert_eq!(*listp, self.freelist);
        self.freelist = p as *mut ();
    }

    unsafe fn allocation_index(&self, ptr: *const T) -> usize {
        let base = &self.objects[0] as *const T;

        // Check that ptr is in range.
        assert!(ptr >= base);
        assert!(ptr < base.offset(HEAP_SIZE as isize));

        let index = (ptr as usize - base as usize) / mem::size_of::<T>();
        assert_eq!(&self.objects[index] as *const T, ptr);
        index
    }

    unsafe fn get_mark_bit(&mut self, ptr: *const T) -> bool {
        let index = self.allocation_index(ptr);
        self.mark_bits[index]
    }

    unsafe fn set_mark_bit(&mut self, ptr: *const T) {
        let index = self.allocation_index(ptr);
        self.mark_bits.set(index, true);
    }

    unsafe fn try_alloc(&mut self) -> Option<*mut T> {
        let p = self.freelist;
        if p.is_null() {
            None
        } else {
            let listp = p as *mut *mut ();
            self.freelist = *listp;
            let ap = p as *mut T;
            let index = self.allocation_index(ap);
            assert!(!self.allocated_bits[index]);
            self.allocated_bits.set(index, true);
            Some(ap)
        }
    }

    unsafe fn sweep(&mut self) {
        for i in 0 .. HEAP_SIZE {
            let p = &mut self.objects[i] as *mut T;
            if self.allocated_bits[i] && !self.mark_bits[i] {
                // The next statement has the effect of calling p's destructor.
                ptr::read(p);
                self.allocated_bits.set(i, false);
                self.add_to_free_list(p);
            }
        }
    }
}

pub struct Heap<'a> {
    id: HeapId<'a>,
    storage: Option<Box<HeapStorage<'a, PairStorage<'a>>>>,  // XXX BOGUS
    pins: RefCell<HashMap<*mut (), usize>>
}

/// Create a heap, pass it to a callback, then destroy the heap.
///
/// The heap's lifetime is directly tied to this function call, for safety. (So
/// the API is a little wonky --- but how many heaps were you planning on
/// creating?)
pub fn with_heap<F, O>(f: F) -> O
    where F: for<'a> FnOnce(&mut Heap<'a>) -> O
{
    f(&mut Heap::new())
}

/// Trait implemented by all types that can be stored directly in the GC heap:
/// the `Storage` types associated with any `HeapInline` or `HeapRef` type.
///
/// XXX maybe this should not be its own trait - fold into HeapInline?
///
pub unsafe trait Mark<'a> {
    unsafe fn mark(ptr: *mut Self);
}

/// Non-inlined function that serves as an entry point to marking. This is used
/// for marking root set entries.
unsafe fn mark_entry_point<'a, T: Mark<'a>>(addr: *mut ()) {
    Mark::mark(addr as *mut T);
}


/// Trait implemented by all types that can be stored in fields of structs (or,
/// eventually, elements of GCVecs) that are stored in the GC heap.
///
/// This trait is unsafe to implement for several reasons, ranging from:
///
/// *   The common-sense: API users aren't supposed to know or care about this.
///     It is only public so that the public macros can see it.
///     Use `gc_ref_type!` and `gc_inline_enum!`.
///
/// *   To the obvious: `Storage` objects are full of pointers and if `to_heap`
///     puts garbage into them, GC will crash.
///
/// *   To the subtle: `from_heap` receives a non-mut reference to a heap
///     value. But there may exist gc-references to that value, in which case
///     `from_heap` (or other code it calls) could modify the value while this
///     direct, non-mut reference exists, which could lead to crashes (due to
///     changing enums if nothing else) - all without using any unsafe code.
///
pub unsafe trait HeapInline<'a> {
    /// The type of the value when it is physically stored in the heap.
    type Storage;

    /// Extract the value from the heap. Do not under any circumstances call
    /// this.  It is for macro-generated code to call; it is impossible for
    /// ordinary users to call this safely, because `ptr` must be a direct,
    /// unwrapped reference to a value stored in the GC heap, which ordinary
    /// users cannot obtain.
    ///
    /// This turns any raw pointers in the `Storage` value into safe
    /// references, so while it's a dangerous function, the result of a correct
    /// call can be safely handed out to user code.
    ///
    unsafe fn from_heap(ptr: &Self::Storage) -> Self;

    /// Convert the value to the form it should have in the heap.
    /// This is for macro-generated code to call.
    fn to_heap(self) -> Self::Storage;
}

impl<'a> Heap<'a> {
    fn new() -> Heap<'a> {
        Heap {
            id: PhantomData,
            storage: None,
            pins: RefCell::new(HashMap::new())
        }
    }

    fn get_storage<T: GCThing<'a>>(&mut self) -> &mut Box<HeapStorage<'a, T>> {
        match self.storage {
            Some(ref mut storage) => return unsafe { mem::transmute(storage) },
            None => ()
        }

        // XXX BOGUS
        let mark_t = mark_entry_point::<T> as unsafe fn (*mut ());
        let mark_pair = mark_entry_point::<PairStorage> as unsafe fn (*mut ());
        if mark_t == mark_pair {
            self.storage = Some(unsafe {
                HeapStorage::new(self as *mut Heap<'a>)
            });
        }

        match self.storage {
            Some(ref mut storage) => return unsafe { mem::transmute(storage) },
            None => panic!("Heap::get_storage: unsupported type (sorry!)")
        }
    }

    /// Add the object `*p` to the root set, protecting it from GC.
    ///
    /// An object that has been pinned *n* times stays in the root set
    /// until it has been unpinned *n* times.
    ///
    /// (Unsafe because if the argument is garbage, a later GC will
    /// crash. Called only from `impl PinnedRef`.)
    unsafe fn pin<T: GCThing<'a>>(&self, p: *mut T) {
        let p = p as *mut ();
        let mut pins = self.pins.borrow_mut();
        let entry = pins.entry(p).or_insert(0);
        *entry += 1;
    }

    /// Unpin an object (see `pin`).
    ///
    /// (Unsafe because unpinning an object that other code is still using
    /// causes dangling pointers. Called only from `impl PinnedRef`.)
    unsafe fn unpin<T: GCThing<'a>>(&self, p: *mut T) {
        let p = p as *mut ();
        let mut pins = self.pins.borrow_mut();
        if {
            let entry = pins.entry(p as *mut ()).or_insert(0);
            assert!(*entry != 0);
            *entry -= 1;
            *entry == 0
        } {
            pins.remove(&p);
        }
    }

    pub fn try_alloc<T: GCRef<'a>>(&mut self, fields: T::Fields) -> Option<T> {
        unsafe {
            let alloc = self.get_storage::<T::ReferentStorage>().try_alloc();
            alloc
                .or_else(|| {
                    self.gc();
                    self.get_storage::<T::ReferentStorage>().try_alloc()
                })
                .map(move |p| {
                    ptr::write(p, T::fields_to_heap(fields));
                    T::from_pinned_ref(PinnedRef::new(p))
                })
        }
    }

    unsafe fn find_header<T: GCThing<'a>>(ptr: *const T) -> *mut HeapStorage<'a, T> {
        let storage_addr = ptr as usize & !(HEAP_STORAGE_ALIGN - 1);
        storage_addr as *mut HeapStorage<'a, T>
    }

    unsafe fn from_allocation<T: GCThing<'a>>(ptr: *const T) -> *const Heap<'a> {
        (*Heap::find_header(ptr)).heap
    }

    pub unsafe fn get_mark_bit<T: GCThing<'a>>(ptr: *const T) -> bool {
        (*Heap::find_header(ptr)).get_mark_bit(ptr)
    }

    pub unsafe fn set_mark_bit<T: GCThing<'a>>(ptr: *const T) {
        (*Heap::find_header(ptr)).set_mark_bit(ptr);
    }
    
    pub fn alloc<T: GCRef<'a>>(&mut self, fields: T::Fields) -> T {
        self.try_alloc(fields).expect("out of memory (gc did not collect anything)")
    }

    unsafe fn mark_any(ptr: *mut ()) {
        let storage = Heap::find_header(ptr as *mut PairStorage);  // BOGUS
        let mark_fn = (*storage).mark_entry_point;
        mark_fn(ptr);
    }

    unsafe fn gc(&mut self) {
        let storage = match self.storage {
            None => return,
            Some(ref mut storage) => storage
        };

        // mark phase
        storage.mark_bits.clear();
        for (&p, _) in self.pins.borrow().iter() {
            Heap::mark_any(p);
        }

        // sweep phase
        storage.sweep();
    }

    #[cfg(test)]
    pub fn force_gc(&mut self) {
        unsafe { self.gc(); }
    }
}

impl<'a> Drop for Heap<'a> {
    fn drop(&mut self) {
        // Perform a final GC to call destructors on any remaining allocations.
        assert!(self.pins.borrow().is_empty());
        unsafe { self.gc(); }
        assert!(match self.storage {
            None => true,
            Some(ref hs) => hs.allocated_bits.none()
        });
    }
}

/// Things that can be allocated in the heap (the backing store for a GCRef type).
pub trait GCThing<'a>: Mark<'a> + Sized + Default {
    type RefType: GCRef<'a, ReferentStorage=Self>;
}

pub struct PinnedRef<'a, T: GCThing<'a>> {
    ptr: *mut T,
    heap_id: HeapId<'a>
}

impl<'a, T: GCThing<'a>> PinnedRef<'a, T> {
    /// Pin an object, returning a new `PinnedRef` that will unpin it when
    /// dropped. Unsafe because if `p` is not a pointer to a live allocation of
    /// type `T` --- and a complete allocation, not a sub-object of one --- then
    /// later unsafe code will explode.
    unsafe fn new(p: *mut T) -> PinnedRef<'a, T> {
        let heap = Heap::from_allocation(p);
        (*heap).pin(p);
        PinnedRef {
            ptr: p,
            heap_id: (*heap).id
        }
    }
}

impl<'a, T: GCThing<'a>> Drop for PinnedRef<'a, T> {
    fn drop(&mut self) {
        unsafe {
            let heap = Heap::from_allocation(self.ptr);
            (*heap).unpin(self.ptr);
        }
    }
}

impl<'a, T: GCThing<'a>> Clone for PinnedRef<'a, T> {
    fn clone(&self) -> PinnedRef<'a, T> {
        let &PinnedRef { ptr, heap_id } = self;
        unsafe {
            let heap = Heap::from_allocation(ptr);
            (*heap).pin(ptr);
        }
        PinnedRef {
            ptr: ptr,
            heap_id: heap_id
        }
    }
}

impl<'a, T: GCThing<'a>> fmt::Debug for PinnedRef<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "PinnedRef {{ ptr: {:p} }}", self.ptr)
    }
}

impl<'a, T: GCThing<'a>> PartialEq for PinnedRef<'a, T> {
    fn eq(&self, other: &PinnedRef<'a, T>) -> bool {
        self.ptr == other.ptr
    }
}

impl<'a, T: GCThing<'a>> Eq for PinnedRef<'a, T> {}

pub trait GCRef<'a>: HeapInline<'a> {
    type ReferentStorage: GCThing<'a>;
    type Fields;

    fn from_pinned_ref(r: PinnedRef<'a, Self::ReferentStorage>) -> Self;

    fn fields_to_heap(fields: Self::Fields) -> Self::ReferentStorage;

    #[cfg(test)]
    fn address(&self) -> usize;
}

macro_rules! gc_trivial_impl {
    ($t:ty) => {
        unsafe impl<'a> Mark<'a> for $t {
            unsafe fn mark(_ptr: *mut $t) {}
        }

        unsafe impl<'a> HeapInline<'a> for $t {
            type Storage = Self;
            fn to_heap(self) -> $t { self }
            unsafe fn from_heap(v: &$t) -> $t { (*v).clone() }
        }
    }
}

gc_trivial_impl!(bool);
gc_trivial_impl!(char);
gc_trivial_impl!(i8);
gc_trivial_impl!(u8);
gc_trivial_impl!(i16);
gc_trivial_impl!(u16);
gc_trivial_impl!(i32);
gc_trivial_impl!(u32);
gc_trivial_impl!(i64);
gc_trivial_impl!(u64);
gc_trivial_impl!(isize);
gc_trivial_impl!(usize);
gc_trivial_impl!(f32);
gc_trivial_impl!(f64);

use std::rc::Rc;
gc_trivial_impl!(Rc<String>);

/*
// 'static types are heap-safe because ref types are never 'static.
// Unfortunately I can't make the compiler understand this: the rules
// to prevent conflicting trait impls make this conflict with almost
// everything.
unsafe impl<'a, T: Clone + 'static> Mark<'a> for T { ...trivial... }
unsafe impl<'a, T: Clone + 'static> HeapInline<'a> for T { ...trivial... }
*/


// === Pair, the reference type

gc_ref_type! {
    pub struct Pair / PairFields / PairStorage<'a> {
        head / set_head: Value<'a>,
        tail / set_tail: Value<'a>
    }
}

impl<'a> Default for PairStorage<'a> {
    fn default() -> PairStorage<'a> {
        PairStorage {
            head: ValueStorage::Null,
            tail: ValueStorage::Null
        }
    }
}



// === Values (a heap-inline enum)

gc_inline_enum! {
    pub enum Value / ValueStorage <'a> {
        Null,
        Int(i32),
        Str(Rc<String>),  // <-- equality is by value
        Pair(Pair<'a>)  // <-- equality is by pointer
    }
}
