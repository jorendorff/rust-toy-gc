//! A Box can be a field of a heap struct only if the boxed type has 'static lifetime.

// error-pattern: cannot infer an appropriate lifetime for lifetime parameter `'h` due to conflicting requirements

#[macro_use] extern crate cell_gc;
#[macro_use] extern crate cell_gc_derive;
mod pairs_aux;
use cell_gc::*;
use pairs_aux::*;

#[derive(IntoHeap)]
struct Thing<'h> {
    boxed_ref: Box<Option<ThingRef<'h>>>
}

fn main() {
    with_heap(|heap| {
        let thing_1 = heap.alloc(Thing { boxed_ref: Box::new(None) });
        let thing_2 = heap.alloc(Thing { boxed_ref: Box::new(Some(thing_1)) });
        std::mem::drop(thing_1);
        heap.force_gc();  // Boxes aren't marked; thing_1 is collected!
        let thing_1_revived = (*thing_2.boxed_ref()).unwrap();  // bad
    });
}
