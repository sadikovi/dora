use parking_lot::MutexGuard;
use std::cmp;

use ctxt::VM;
use gc::root::Slot;
use gc::space::Space;
use gc::swiper::card::CardTable;
use gc::swiper::crossing::CrossingMap;
use gc::swiper::large::LargeSpace;
use gc::swiper::old::{OldGen, OldGenProtected};
use gc::swiper::walk_region;
use gc::swiper::young::YoungGen;
use gc::{Address, GcReason, Region};
use object::Obj;

pub struct FullCollector<'a, 'ast: 'a> {
    vm: &'a VM<'ast>,
    heap: Region,
    young: &'a YoungGen,
    old: &'a OldGen,
    old_protected: MutexGuard<'a, OldGenProtected>,
    large_space: &'a LargeSpace,
    rootset: &'a [Slot],
    card_table: &'a CardTable,
    crossing_map: &'a CrossingMap,
    perm_space: &'a Space,

    old_top: Address,
    old_limit: Address,
    old_committed: Region,
    init_old_top: Vec<Address>,

    reason: GcReason,

    min_heap_size: usize,
    max_heap_size: usize,
}

impl<'a, 'ast> FullCollector<'a, 'ast> {
    pub fn new(
        vm: &'a VM<'ast>,
        heap: Region,
        young: &'a YoungGen,
        old: &'a OldGen,
        large_space: &'a LargeSpace,
        card_table: &'a CardTable,
        crossing_map: &'a CrossingMap,
        perm_space: &'a Space,
        rootset: &'a [Slot],
        reason: GcReason,
        min_heap_size: usize,
        max_heap_size: usize,
    ) -> FullCollector<'a, 'ast> {
        let old_total = old.total();

        FullCollector {
            vm: vm,
            heap: heap,
            young: young,
            old: old,
            old_protected: old.protected(),
            large_space: large_space,
            rootset: rootset,
            card_table: card_table,
            crossing_map: crossing_map,
            perm_space: perm_space,

            old_top: old_total.start,
            old_limit: old_total.end,
            old_committed: Default::default(),
            init_old_top: Vec::new(),

            reason: reason,

            min_heap_size: min_heap_size,
            max_heap_size: max_heap_size,
        }
    }

    pub fn collect(&mut self) {
        let dev_verbose = self.vm.args.flag_gc_dev_verbose;
        self.init_old_top = self.old_protected.regions.iter().map(|r| r.top()).collect();

        if dev_verbose {
            println!("Full GC: Phase 1 (marking)");
        }

        self.mark_live();

        if self.vm.args.flag_gc_verify {
            if dev_verbose {
                println!("Full GC: Phase 1 (verify marking start)");
            }

            verify_marking(
                self.young,
                &*self.old_protected,
                self.large_space,
                self.heap,
            );

            if dev_verbose {
                println!("Full GC: Phase 1 (verify marking end)");
            }
        }

        if dev_verbose {
            println!("Full GC: Phase 2 (compute forward)");
        }

        self.compute_forward();

        if dev_verbose {
            println!("Full GC: Phase 3 (update refs)");
        }

        self.update_references();

        if dev_verbose {
            println!("Full GC: Phase 4 (relocate)");
        }

        self.relocate();

        if dev_verbose {
            println!("Full GC: Phase 5 (large objects)");
        }

        self.update_large_objects();

        if dev_verbose {
            println!("Full GC: Phase 5 (large objects) finished.");
        }

        self.reset_cards();

        self.young.clear();
        self.young.protect_to();

        self.old_protected.update_single_region(self.old_top);
    }

    fn mark_live(&mut self) {
        let mut marking_stack: Vec<Address> = Vec::new();

        for root in self.rootset {
            let root_ptr = root.get();

            if self.heap.contains(root_ptr) {
                let root_obj = root_ptr.to_mut_obj();

                if !root_obj.header().is_marked_non_atomic() {
                    marking_stack.push(root_ptr);
                    root_obj.header_mut().mark_non_atomic();
                }
            } else {
                debug_assert!(root_ptr.is_null() || self.perm_space.contains(root_ptr));
            }
        }

        while marking_stack.len() > 0 {
            let object_addr = marking_stack.pop().expect("stack already empty");
            let object = object_addr.to_mut_obj();

            object.visit_reference_fields(|field| {
                let field_addr = field.get();

                if self.heap.contains(field_addr) {
                    let field_obj = field_addr.to_mut_obj();

                    if !field_obj.header().is_marked_non_atomic() {
                        marking_stack.push(field_addr);
                        field_obj.header_mut().mark_non_atomic();
                    }
                } else {
                    debug_assert!(field_addr.is_null() || self.perm_space.contains(field_addr));
                }
            });
        }
    }

    fn compute_forward(&mut self) {
        self.walk_old_and_young(|full, object, _address, object_size| {
            if object.header().is_marked_non_atomic() {
                let fwd = full.allocate(object_size);
                object.header_mut().set_fwdptr_non_atomic(fwd);
            }
        });

        self.old_protected.commit_single_region(self.old_top);
        self.old_committed = Region::new(self.old.total_start(), self.old_top);
    }

    fn update_references(&mut self) {
        self.walk_old_and_young(|full, object, _address, _| {
            if object.header().is_marked_non_atomic() {
                object.visit_reference_fields(|field| {
                    full.forward_reference(field);
                });
            }
        });

        for root in self.rootset {
            self.forward_reference(*root);
        }

        self.large_space.visit_objects(|object_start| {
            let object = object_start.to_mut_obj();

            if object.header().is_marked_non_atomic() {
                object.visit_reference_fields(|field| {
                    self.forward_reference(field);
                });
            }
        });
    }

    fn relocate(&mut self) {
        self.crossing_map.set_first_object(0.into(), 0);

        self.walk_old_and_young(|full, object, address, object_size| {
            if object.header().is_marked_non_atomic() {
                // get new location
                let dest = object.header().fwdptr_non_atomic();
                debug_assert!(full.old_committed.contains(dest));

                // determine location after relocated object
                let next_dest = dest.offset(object_size);
                debug_assert!(full.old_committed.valid_top(next_dest));

                if address != dest {
                    object.copy_to(dest, object_size);
                }

                // unmark object for next collection
                let dest_obj = dest.to_mut_obj();
                dest_obj.header_mut().unmark_non_atomic();

                full.old
                    .update_crossing(dest, next_dest, dest_obj.is_array_ref());
            }
        });
    }

    fn update_large_objects(&mut self) {
        self.large_space.remove_objects(|object_start| {
            let object = object_start.to_mut_obj();

            // reset cards for object, also do this for dead objects
            // to reset card entries to clean.
            if object.is_array_ref() {
                let object_end = object_start.offset(object.size());
                self.card_table.reset_region(object_start, object_end);
            } else {
                self.card_table.reset_addr(object_start);
            }

            if !object.header().is_marked_non_atomic() {
                // object is unmarked -> free it
                return false;
            }

            // unmark object for next collection
            object.header_mut().unmark_non_atomic();

            // keep object
            true
        });
    }

    fn reset_cards(&mut self) {
        let regions = self
            .old_protected
            .regions
            .iter()
            .map(|r| (r.start(), r.top()))
            .collect::<Vec<_>>();

        for ((start, top), init_top) in regions.into_iter().zip(&self.init_old_top) {
            let top = cmp::max(top, *init_top);
            self.card_table.reset_region(start, top);
        }
    }

    fn forward_reference(&mut self, slot: Slot) {
        let object_addr = slot.get();

        if self.heap.contains(object_addr) {
            debug_assert!(object_addr.to_obj().header().is_marked_non_atomic());

            if self.large_space.contains(object_addr) {
                // large objects do not move in memory
                return;
            }

            let fwd_addr = object_addr.to_obj().header().fwdptr_non_atomic();
            debug_assert!(self.heap.contains(fwd_addr));
            slot.set(fwd_addr);
        } else {
            debug_assert!(object_addr.is_null() || self.perm_space.contains(object_addr));
        }
    }

    fn walk_old_and_young<F>(&mut self, mut fct: F)
    where
        F: FnMut(&mut FullCollector, &mut Obj, Address, usize),
    {
        let old_regions = self
            .old_protected
            .regions
            .iter()
            .map(|r| r.active_region())
            .collect::<Vec<_>>();

        for old_region in old_regions {
            walk_region(old_region, |obj, addr, size| {
                fct(self, obj, addr, size);
            });
        }

        let used_region = self.young.eden_active();
        walk_region(used_region, |obj, addr, size| {
            fct(self, obj, addr, size);
        });

        let used_region = self.young.from_active();
        walk_region(used_region, |obj, addr, size| {
            fct(self, obj, addr, size);
        });

        // This is a bit strange at first: to-space might not be empty,
        // after too many survivors in the minor GC of the young gen.
        let used_region = self.young.to_active();
        walk_region(used_region, |obj, addr, size| {
            fct(self, obj, addr, size);
        });
    }

    fn allocate(&mut self, object_size: usize) -> Address {
        let addr = self.old_top;
        let next = self.old_top.offset(object_size);

        if next <= self.old_limit {
            self.old_top = next;
            return addr;
        }

        panic!("FAIL: Not enough space for objects in old generation.");
    }
}

pub fn verify_marking(
    young: &YoungGen,
    old_protected: &OldGenProtected,
    large: &LargeSpace,
    heap: Region,
) {
    for region in &old_protected.regions {
        let active = region.active_region();
        verify_marking_region(active, heap);
    }

    let eden = young.eden_active();
    verify_marking_region(eden, heap);

    let from = young.from_active();
    verify_marking_region(from, heap);

    let to = young.to_active();
    verify_marking_region(to, heap);

    large.visit_objects(|obj_address| {
        verify_marking_object(obj_address, heap);
    });
}

fn verify_marking_region(region: Region, heap: Region) {
    walk_region(region, |_obj, obj_address, _size| {
        verify_marking_object(obj_address, heap);
    });
}

fn verify_marking_object(obj_address: Address, heap: Region) {
    let obj = obj_address.to_mut_obj();

    if obj.header().is_marked_non_atomic() {
        obj.visit_reference_fields(|field| {
            let object_addr = field.get();

            if heap.contains(object_addr) {
                assert!(object_addr.to_obj().header().is_marked_non_atomic());
            }
        });
    }
}
