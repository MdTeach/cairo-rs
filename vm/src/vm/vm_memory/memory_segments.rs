use crate::stdlib::prelude::*;
use crate::stdlib::{any::Any, collections::HashMap};
use crate::vm::runners::cairo_runner::CairoArg;

use crate::{
    types::relocatable::{MaybeRelocatable, Relocatable},
    vm::{
        errors::memory_errors::MemoryError, errors::vm_errors::VirtualMachineError,
        vm_memory::memory::Memory,
    },
};

pub struct MemorySegmentManager {
    pub segment_sizes: HashMap<usize, usize>,
    pub segment_used_sizes: Option<Vec<usize>>,
    pub(crate) memory: Memory,
    // A map from segment index to a list of pairs (offset, page_id) that constitute the
    // public memory. Note that the offset is absolute (not based on the page_id).
    pub public_memory_offsets: HashMap<usize, Vec<(usize, usize)>>,
}

impl MemorySegmentManager {
    /// Number of segments in the real memory
    pub fn num_segments(&self) -> usize {
        self.memory.data.len()
    }

    /// Number of segments in the temporary memory
    pub fn num_temp_segments(&self) -> usize {
        self.memory.temp_data.len()
    }

    ///Adds a new segment and returns its starting location as a Relocatable value. Its segment index will always be positive.
    pub fn add(&mut self) -> Relocatable {
        self.memory.data.push(Vec::new());
        Relocatable {
            segment_index: (self.memory.data.len() - 1) as isize,
            offset: 0,
        }
    }

    /// Adds a new temporary segment and returns its starting location as a Relocatable value. Its segment index will always be negative.
    pub fn add_temporary_segment(&mut self) -> Relocatable {
        self.memory.temp_data.push(Vec::new());
        Relocatable {
            // We dont substract 1 as we need to take into account the index shift (temporary memory begins from -1 instead of 0)
            segment_index: -((self.memory.temp_data.len()) as isize),
            offset: 0,
        }
    }

    ///Writes data into the memory from address ptr and returns the first address after the data.
    pub fn load_data(
        &mut self,
        ptr: Relocatable,
        data: &Vec<MaybeRelocatable>,
    ) -> Result<Relocatable, MemoryError> {
        // Starting from the end ensures any necessary resize
        // is performed once with enough room for everything
        for (num, value) in data.iter().enumerate().rev() {
            self.memory.insert((ptr + num)?, value)?;
        }
        (ptr + data.len()).map_err(MemoryError::Math)
    }

    pub fn new() -> MemorySegmentManager {
        MemorySegmentManager {
            segment_sizes: HashMap::new(),
            segment_used_sizes: None,
            public_memory_offsets: HashMap::new(),
            memory: Memory::new(),
        }
    }

    /// Calculates the size of each memory segment.
    pub fn compute_effective_sizes(&mut self) -> &Vec<usize> {
        self.segment_used_sizes
            .get_or_insert_with(|| self.memory.data.iter().map(Vec::len).collect())
    }

    ///Returns the number of used segments if they have been computed.
    ///Returns None otherwise.
    pub fn get_segment_used_size(&self, index: usize) -> Option<usize> {
        self.segment_used_sizes.as_ref()?.get(index).copied()
    }

    pub fn get_segment_size(&self, index: usize) -> Option<usize> {
        self.segment_sizes
            .get(&index)
            .cloned()
            .or_else(|| self.get_segment_used_size(index))
    }

    ///Returns a vector containing the first relocated address of each memory segment
    pub fn relocate_segments(&self) -> Result<Vec<usize>, MemoryError> {
        let first_addr = 1;
        let mut relocation_table = vec![first_addr];
        match &self.segment_used_sizes {
            Some(segment_used_sizes) => {
                for (i, _size) in segment_used_sizes.iter().enumerate() {
                    let segment_size = self
                        .get_segment_size(i)
                        .ok_or(MemoryError::MissingSegmentUsedSizes)?;

                    relocation_table.push(relocation_table[i] + segment_size);
                }
            }
            None => return Err(MemoryError::MissingSegmentUsedSizes),
        }
        //The last value corresponds to the total amount of elements across all segments, which isnt needed for relocation.
        relocation_table.pop();
        Ok(relocation_table)
    }

    pub fn gen_arg(&mut self, arg: &dyn Any) -> Result<MaybeRelocatable, MemoryError> {
        if let Some(value) = arg.downcast_ref::<MaybeRelocatable>() {
            Ok(value.clone())
        } else if let Some(value) = arg.downcast_ref::<Vec<MaybeRelocatable>>() {
            let base = self.add();
            self.write_arg(base, value)?;
            Ok(base.into())
        } else if let Some(value) = arg.downcast_ref::<Vec<Relocatable>>() {
            let base = self.add();
            self.write_arg(base, value)?;
            Ok(base.into())
        } else {
            Err(MemoryError::GenArgInvalidType)
        }
    }

    pub fn gen_cairo_arg(
        &mut self,
        arg: &CairoArg,
    ) -> Result<MaybeRelocatable, VirtualMachineError> {
        match arg {
            CairoArg::Single(value) => Ok(value.clone()),
            CairoArg::Array(values) => {
                let base = self.add();
                self.load_data(base, values)?;
                Ok(base.into())
            }
            CairoArg::Composed(cairo_args) => {
                let args = cairo_args
                    .iter()
                    .map(|cairo_arg| self.gen_cairo_arg(cairo_arg))
                    .collect::<Result<Vec<MaybeRelocatable>, VirtualMachineError>>()?;
                let base = self.add();
                self.load_data(base, &args)?;
                Ok(base.into())
            }
        }
    }

    pub fn write_arg(
        &mut self,
        ptr: Relocatable,
        arg: &dyn Any,
    ) -> Result<MaybeRelocatable, MemoryError> {
        if let Some(vector) = arg.downcast_ref::<Vec<MaybeRelocatable>>() {
            self.load_data(ptr, vector).map(Into::into)
        } else if let Some(vector) = arg.downcast_ref::<Vec<Relocatable>>() {
            let data = &vector.iter().map(|value| value.into()).collect();
            self.load_data(ptr, data).map(Into::into)
        } else {
            Err(MemoryError::WriteArg)
        }
    }

    pub fn is_valid_memory_value(&self, value: &MaybeRelocatable) -> Result<bool, MemoryError> {
        match &self.segment_used_sizes {
            Some(segment_used_sizes) => match value {
                MaybeRelocatable::Int(_) => Ok(true),
                MaybeRelocatable::RelocatableValue(relocatable) => {
                    let segment_index: usize =
                        relocatable.segment_index.try_into().map_err(|_| {
                            MemoryError::AddressInTemporarySegment(relocatable.segment_index)
                        })?;

                    Ok(segment_index < segment_used_sizes.len())
                }
            },
            None => Err(MemoryError::MissingSegmentUsedSizes),
        }
    }

    pub fn get_memory_holes(&self, builtin_count: usize) -> Result<usize, MemoryError> {
        let data = &self.memory.data;
        let mut memory_holes = 0;
        let builtin_segments_start = 1; // program segment + execution segment
        let builtin_segments_end = builtin_segments_start + builtin_count;
        // Count the memory holes for each segment by substracting the amount of accessed_addresses from the segment's size
        // Segments without accesses addresses are not accounted for when counting memory holes
        for i in 0..data.len() {
            // Instead of marking all of the builtin segment's address as accessed, we just skip them when counting memory holes
            if i > builtin_segments_start && i <= builtin_segments_end {
                continue;
            }
            let accessed_amount = match self.memory.get_amount_of_accessed_addresses_for_segment(i)
            {
                Some(accessed_amount) if accessed_amount > 0 => accessed_amount,
                _ => continue,
            };
            let segment_size = self
                .get_segment_size(i)
                .ok_or(MemoryError::MissingSegmentUsedSizes)?;
            if accessed_amount > segment_size {
                return Err(MemoryError::SegmentHasMoreAccessedAddressesThanSize(
                    Box::new((i, accessed_amount, segment_size)),
                ));
            }
            memory_holes += segment_size - accessed_amount;
        }
        Ok(memory_holes)
    }

    // Writes the following information for the given segment:
    // * size - The size of the segment (to be used in relocate_segments).
    // * public_memory - A list of offsets for memory cells that will be considered as public
    // memory.
    pub(crate) fn finalize(
        &mut self,
        size: Option<usize>,
        segment_index: usize,
        public_memory: Option<&Vec<(usize, usize)>>,
    ) {
        if let Some(size) = size {
            self.segment_sizes.insert(segment_index, size);
        }
        if let Some(public_memory) = public_memory {
            self.public_memory_offsets
                .insert(segment_index, public_memory.clone());
        }
    }
}

impl Default for MemorySegmentManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{relocatable, utils::test_utils::*, vm::vm_memory::memory::MemoryCell};
    use assert_matches::assert_matches;
    use felt::Felt252;
    use num_traits::Num;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::*;

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn add_segment_no_size() {
        let mut segments = MemorySegmentManager::new();
        let base = segments.add();
        assert_eq!(base, relocatable!(0, 0));
        assert_eq!(segments.num_segments(), 1);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn add_segment_no_size_test_two_segments() {
        let mut segments = MemorySegmentManager::new();
        let mut _base = segments.add();
        _base = segments.add();
        assert_eq!(
            _base,
            Relocatable {
                segment_index: 1,
                offset: 0
            }
        );
        assert_eq!(segments.num_segments(), 2);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn add_one_temporary_segment() {
        let mut segments = MemorySegmentManager::new();
        let base = segments.add_temporary_segment();
        assert_eq!(base, relocatable!(-1, 0));
        assert_eq!(segments.num_temp_segments(), 1);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn add_two_temporary_segments() {
        let mut segments = MemorySegmentManager::new();
        segments.add_temporary_segment();
        let base = segments.add_temporary_segment();
        assert_eq!(
            base,
            Relocatable {
                segment_index: -2,
                offset: 0
            }
        );
        assert_eq!(segments.num_temp_segments(), 2);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn load_data_empty() {
        let data = Vec::new();
        let ptr = Relocatable::from((0, 3));
        let mut segments = MemorySegmentManager::new();
        let current_ptr = segments.load_data(ptr, &data).unwrap();
        assert_eq!(current_ptr, Relocatable::from((0, 3)));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn load_data_one_element() {
        let data = vec![MaybeRelocatable::from(Felt252::new(4))];
        let ptr = Relocatable::from((0, 0));
        let mut segments = MemorySegmentManager::new();
        segments.add();
        let current_ptr = segments.load_data(ptr, &data).unwrap();
        assert_eq!(current_ptr, Relocatable::from((0, 1)));
        assert_eq!(
            segments.memory.get(&ptr).unwrap().as_ref(),
            &MaybeRelocatable::from(Felt252::new(4))
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn load_data_three_elements() {
        let data = vec![
            MaybeRelocatable::from(Felt252::new(4)),
            MaybeRelocatable::from(Felt252::new(5)),
            MaybeRelocatable::from(Felt252::new(6)),
        ];
        let ptr = Relocatable::from((0, 0));
        let mut segments = MemorySegmentManager::new();
        segments.add();
        let current_ptr = segments.load_data(ptr, &data).unwrap();
        assert_eq!(current_ptr, Relocatable::from((0, 3)));

        assert_eq!(
            segments.memory.get(&ptr).unwrap().as_ref(),
            &MaybeRelocatable::from(Felt252::new(4))
        );
        assert_eq!(
            segments
                .memory
                .get(&MaybeRelocatable::from((0, 1)))
                .unwrap()
                .as_ref(),
            &MaybeRelocatable::from(Felt252::new(5))
        );
        assert_eq!(
            segments
                .memory
                .get(&MaybeRelocatable::from((0, 2)))
                .unwrap()
                .as_ref(),
            &MaybeRelocatable::from(Felt252::new(6))
        );
    }
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_effective_sizes_for_one_segment_memory() {
        let mut segments = segments![((0, 0), 1), ((0, 1), 1), ((0, 2), 1)];
        segments.compute_effective_sizes();
        assert_eq!(Some(vec![3]), segments.segment_used_sizes);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_effective_sizes_for_one_segment_memory_with_gap() {
        let mut segments = MemorySegmentManager::new();
        segments.add();
        segments
            .memory
            .insert(
                Relocatable::from((0, 6)),
                &MaybeRelocatable::from(Felt252::new(1)),
            )
            .unwrap();
        segments.compute_effective_sizes();
        assert_eq!(Some(vec![7]), segments.segment_used_sizes);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_effective_sizes_for_one_segment_memory_with_gaps() {
        let mut segments = segments![((0, 3), 1), ((0, 4), 1), ((0, 7), 1), ((0, 9), 1)];
        segments.compute_effective_sizes();
        assert_eq!(Some(vec![10]), segments.segment_used_sizes);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_effective_sizes_for_three_segment_memory() {
        let mut segments = segments![
            ((0, 0), 1),
            ((0, 1), 1),
            ((0, 2), 1),
            ((1, 0), 1),
            ((1, 1), 1),
            ((1, 2), 1),
            ((2, 0), 1),
            ((2, 1), 1),
            ((2, 2), 1)
        ];
        segments.compute_effective_sizes();
        assert_eq!(Some(vec![3, 3, 3]), segments.segment_used_sizes);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn compute_effective_sizes_for_three_segment_memory_with_gaps() {
        let mut segments = segments![
            ((0, 2), 1),
            ((0, 5), 1),
            ((0, 7), 1),
            ((1, 1), 1),
            ((2, 2), 1),
            ((2, 4), 1),
            ((2, 7), 1)
        ];
        segments.compute_effective_sizes();
        assert_eq!(Some(vec![8, 2, 8]), segments.segment_used_sizes);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_segment_used_size_after_computing_used() {
        let mut segments = segments![
            ((0, 2), 1),
            ((0, 5), 1),
            ((0, 7), 1),
            ((1, 1), 1),
            ((2, 2), 1),
            ((2, 4), 1),
            ((2, 7), 1)
        ];
        segments.compute_effective_sizes();
        assert_eq!(Some(8), segments.get_segment_used_size(2));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_segment_used_size_before_computing_used() {
        let segments = MemorySegmentManager::new();
        assert_eq!(None, segments.get_segment_used_size(2));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn relocate_segments_one_segment() {
        let mut segments = MemorySegmentManager::new();
        segments.segment_used_sizes = Some(vec![3]);
        assert_eq!(
            segments
                .relocate_segments()
                .expect("Couldn't relocate after compute effective sizes"),
            vec![1]
        )
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn relocate_segments_five_segment() {
        let mut segments = MemorySegmentManager::new();
        segments.segment_used_sizes = Some(vec![3, 3, 56, 78, 8]);
        assert_eq!(
            segments
                .relocate_segments()
                .expect("Couldn't relocate after compute effective sizes"),
            vec![1, 4, 7, 63, 141]
        )
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn write_arg_with_apply_modulo() {
        let mut big_num = num_bigint::BigInt::from_str_radix(&felt::PRIME_STR[2..], 16)
            .expect("Couldn't parse prime");
        big_num += 1;
        let big_maybe_rel = MaybeRelocatable::from(Felt252::new(big_num));
        let data = vec![mayberelocatable!(11), mayberelocatable!(12), big_maybe_rel];
        let ptr = Relocatable::from((1, 0));
        let mut segments = MemorySegmentManager::new();
        for _ in 0..2 {
            segments.add();
        }

        let exec = segments.write_arg(ptr, &data);

        assert_eq!(exec, Ok(MaybeRelocatable::from((1, 3))));
        assert_eq!(
            segments.memory.data[1],
            vec![
                Some(MemoryCell::new(mayberelocatable!(11))),
                Some(MemoryCell::new(mayberelocatable!(12))),
                Some(MemoryCell::new(mayberelocatable!(1))),
            ]
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn write_arg_relocatable() {
        let data = vec![
            Relocatable::from((0, 1)),
            Relocatable::from((0, 2)),
            Relocatable::from((0, 3)),
        ];
        let ptr = Relocatable::from((1, 0));
        let mut segments = MemorySegmentManager::new();
        for _ in 0..2 {
            segments.add();
        }

        let exec = segments.write_arg(ptr, &data);

        assert_eq!(exec, Ok(MaybeRelocatable::from((1, 3))));
        assert_eq!(
            segments.memory.data[1],
            vec![
                Some(MemoryCell::new(MaybeRelocatable::from((0, 1)))),
                Some(MemoryCell::new(MaybeRelocatable::from((0, 2)))),
                Some(MemoryCell::new(MaybeRelocatable::from((0, 3)))),
            ]
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn segment_default() {
        let segment_mng_new = MemorySegmentManager::new();
        let segment_mng_def: MemorySegmentManager = Default::default();
        assert_eq!(
            segment_mng_new.num_segments(),
            segment_mng_def.num_segments()
        );
        assert_eq!(
            segment_mng_new.segment_used_sizes,
            segment_mng_def.segment_used_sizes
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn is_valid_memory_value_missing_effective_sizes() {
        let segment_manager = MemorySegmentManager::new();

        assert_eq!(
            segment_manager.is_valid_memory_value(&mayberelocatable!(0)),
            Err(MemoryError::MissingSegmentUsedSizes),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn is_valid_memory_value_temporary_segment() {
        let mut segment_manager = MemorySegmentManager::new();

        segment_manager.segment_used_sizes = Some(vec![10]);
        assert_eq!(
            segment_manager.is_valid_memory_value(&mayberelocatable!(-1, 0)),
            Err(MemoryError::AddressInTemporarySegment(-1)),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn is_valid_memory_value_invalid_segment() {
        let mut segment_manager = MemorySegmentManager::new();

        segment_manager.segment_used_sizes = Some(vec![10]);
        assert_eq!(
            segment_manager.is_valid_memory_value(&mayberelocatable!(1, 0)),
            Ok(false),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn is_valid_memory_value() {
        let mut segment_manager = MemorySegmentManager::new();

        segment_manager.segment_used_sizes = Some(vec![10]);
        assert_eq!(
            segment_manager.is_valid_memory_value(&mayberelocatable!(0, 5)),
            Ok(true),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_holes_missing_segment_used_sizes() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.memory = memory![((0, 0), 0)];
        memory_segment_manager
            .memory
            .mark_as_accessed((0, 0).into());
        assert_eq!(
            memory_segment_manager.get_memory_holes(0),
            Err(MemoryError::MissingSegmentUsedSizes),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_holes_out_of_address_offset_bigger_than_size() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_used_sizes = Some(vec![2]);
        memory_segment_manager.memory = memory![((0, 0), 1), ((0, 1), 1), ((0, 2), 2)];
        for i in 0..3 {
            memory_segment_manager
                .memory
                .mark_as_accessed((0, i).into());
        }
        assert_eq!(
            memory_segment_manager.get_memory_holes(0),
            Err(MemoryError::SegmentHasMoreAccessedAddressesThanSize(
                Box::new((0, 3, 2))
            )),
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_holes_empty() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_used_sizes = Some(Vec::new());
        assert_eq!(memory_segment_manager.get_memory_holes(0), Ok(0),);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_holes_empty2() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_used_sizes = Some(vec![4]);
        assert_eq!(memory_segment_manager.get_memory_holes(0), Ok(0),);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_holes() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_used_sizes = Some(vec![10]);
        memory_segment_manager.memory = memory![
            ((0, 0), 0),
            ((0, 1), 0),
            ((0, 2), 0),
            ((0, 3), 0),
            ((0, 6), 0),
            ((0, 7), 0),
            ((0, 8), 0),
            ((0, 9), 0)
        ];
        for i in [0, 1, 2, 3, 6, 7, 8, 9] {
            memory_segment_manager
                .memory
                .mark_as_accessed((0, i).into());
        }
        assert_eq!(memory_segment_manager.get_memory_holes(0), Ok(2),);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_holes2() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        memory_segment_manager.segment_sizes = HashMap::from([(0, 15)]);
        memory_segment_manager.memory = memory![
            ((0, 0), 0),
            ((0, 1), 0),
            ((0, 2), 0),
            ((0, 3), 0),
            ((0, 6), 0),
            ((0, 7), 0),
            ((0, 8), 0),
            ((0, 9), 0)
        ];
        memory_segment_manager.segment_used_sizes = Some(vec![10]);
        for i in [0, 1, 2, 3, 6, 7, 8, 9] {
            memory_segment_manager
                .memory
                .mark_as_accessed((0, i).into());
        }
        assert_eq!(memory_segment_manager.get_memory_holes(0), Ok(7),);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_size_missing_segment() {
        let memory_segment_manager = MemorySegmentManager::new();

        assert_eq!(memory_segment_manager.get_segment_size(0), None);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_size_used() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_used_sizes = Some(vec![5]);

        assert_eq!(memory_segment_manager.get_segment_size(0), Some(5));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_size() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_sizes = HashMap::from([(0, 5)]);

        assert_eq!(memory_segment_manager.get_segment_size(0), Some(5));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn get_memory_size2() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        memory_segment_manager.segment_sizes = HashMap::from([(0, 5)]);
        memory_segment_manager.segment_used_sizes = Some(vec![3]);

        assert_eq!(memory_segment_manager.get_segment_size(0), Some(5));
    }

    /// Test that the call to .gen_arg() with a relocatable just passes the
    /// value through.
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_arg_relocatable() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_arg(&mayberelocatable!(0, 0)),
            Ok(x) if x == mayberelocatable!(0, 0)
        );
    }

    /// Test that the call to .gen_arg() with a bigint and no prime number just
    /// passes the value through.
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_arg_bigint() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_arg(&mayberelocatable!(1234)),
            Ok(x) if x == mayberelocatable!(1234)
        );
    }

    /// Test that the call to .gen_arg() with a Vec<MaybeRelocatable> writes its
    /// contents into a new segment and returns a pointer to it.
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_arg_vec() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_arg(
                &vec![
                    mayberelocatable!(0),
                    mayberelocatable!(1),
                    mayberelocatable!(2),
                    mayberelocatable!(3),
                    mayberelocatable!(0, 0),
                    mayberelocatable!(0, 1),
                    mayberelocatable!(0, 2),
                    mayberelocatable!(0, 3),
                ],
            ),
            Ok(x) if x == mayberelocatable!(0, 0)
        );
    }

    /// Test that the call to .gen_arg() with a Vec<Relocatable> writes its
    /// contents into a new segment and returns a pointer to it.
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_arg_vec_relocatable() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_arg(
                &vec![
                    MaybeRelocatable::from((0, 0)),
                    MaybeRelocatable::from((0, 1)),
                    MaybeRelocatable::from((0, 2)),
                    MaybeRelocatable::from((0, 3)),
                ],
            ),
            Ok(x) if x == mayberelocatable!(0, 0)
        );
    }

    /// Test that the call to .gen_arg() with any other argument returns a not
    /// implemented error.
    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_arg_invalid_type() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_arg(&""),
            Err(MemoryError::GenArgInvalidType)
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn finalize_no_size_nor_memory_no_change() {
        let mut segments = MemorySegmentManager::new();
        segments.finalize(None, 0, None);
        assert!(segments.memory.data.is_empty());
        assert!(segments.memory.temp_data.is_empty());
        assert!(segments.public_memory_offsets.is_empty());
        assert_eq!(segments.num_segments(), 0);
        assert_eq!(segments.num_temp_segments(), 0);
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn finalize_no_memory() {
        let mut segments = MemorySegmentManager::new();
        segments.finalize(Some(42), 0, None);
        assert!(segments.public_memory_offsets.is_empty());
        assert_eq!(segments.segment_sizes, HashMap::from([(0, 42)]));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn finalize_no_size() {
        let mut segments = MemorySegmentManager::new();
        segments.finalize(None, 0, Some(&vec![(1_usize, 2_usize)]));
        assert_eq!(
            segments.public_memory_offsets,
            HashMap::from([(0_usize, vec![(1_usize, 2_usize)])])
        );
        assert!(segments.segment_sizes.is_empty());
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn finalize_all_args() {
        let mut segments = MemorySegmentManager::new();
        segments.finalize(Some(42), 0, Some(&vec![(1_usize, 2_usize)]));
        assert_eq!(
            segments.public_memory_offsets,
            HashMap::from([(0_usize, vec![(1_usize, 2_usize)])])
        );
        assert_eq!(segments.segment_sizes, HashMap::from([(0, 42)]));
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_cairo_arg_single() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_cairo_arg(&mayberelocatable!(1234).into()),
            Ok(x) if x == mayberelocatable!(1234)
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_cairo_arg_array() {
        let mut memory_segment_manager = MemorySegmentManager::new();

        assert_matches!(
            memory_segment_manager.gen_cairo_arg(
                &vec![
                    mayberelocatable!(0),
                    mayberelocatable!(1),
                    mayberelocatable!(2),
                    mayberelocatable!(3),
                    mayberelocatable!(0, 0),
                    mayberelocatable!(0, 1),
                    mayberelocatable!(0, 2),
                    mayberelocatable!(0, 3),
                ]
                .into(),
            ),
            Ok(x) if x == mayberelocatable!(0, 0)
        );
    }

    #[test]
    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
    fn gen_cairo_arg_composed() {
        let mut memory_segment_manager = MemorySegmentManager::new();
        let cairo_args = CairoArg::Composed(vec![
            CairoArg::Array(vec![
                mayberelocatable!(0),
                mayberelocatable!(1),
                mayberelocatable!(2),
            ]),
            CairoArg::Single(mayberelocatable!(1234)),
            CairoArg::Single(mayberelocatable!(5678)),
            CairoArg::Array(vec![
                mayberelocatable!(3),
                mayberelocatable!(4),
                mayberelocatable!(5),
            ]),
        ]);

        assert_matches!(
            memory_segment_manager.gen_cairo_arg(&cairo_args),
            Ok(x) if x == mayberelocatable!(2, 0)
        );
    }
}
