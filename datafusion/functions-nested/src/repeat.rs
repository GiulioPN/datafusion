// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! [`ScalarUDFImpl`] definitions for array_repeat function.

use crate::utils::make_scalar_function;
use arrow::array::{
    new_null_array, Array, ArrayRef, Capacities, GenericListArray, ListArray,
    MutableArrayData, OffsetSizeTrait, UInt64Array,
};
use arrow::buffer::OffsetBuffer;
use arrow::compute;
use arrow::compute::cast;
use arrow::datatypes::DataType;
use arrow::datatypes::{
    DataType::{LargeList, List},
    Field,
};
use datafusion_common::cast::{as_large_list_array, as_list_array, as_uint64_array};
use datafusion_common::{exec_err, utils::take_function_args, Result};
use datafusion_expr::{
    ColumnarValue, Documentation, ScalarUDFImpl, Signature, Volatility,
};
use datafusion_macros::user_doc;
use std::any::Any;
use std::sync::Arc;

make_udf_expr_and_func!(
    ArrayRepeat,
    array_repeat,
    element count, // arg name
    "returns an array containing element `count` times.", // doc
    array_repeat_udf // internal function name
);

#[user_doc(
    doc_section(label = "Array Functions"),
    description = "Returns an array containing element `count` times.",
    syntax_example = "array_repeat(element, count)",
    sql_example = r#"```sql
> select array_repeat(1, 3);
+---------------------------------+
| array_repeat(Int64(1),Int64(3)) |
+---------------------------------+
| [1, 1, 1]                       |
+---------------------------------+
> select array_repeat([1, 2], 2);
+------------------------------------+
| array_repeat(List([1,2]),Int64(2)) |
+------------------------------------+
| [[1, 2], [1, 2]]                   |
+------------------------------------+
```"#,
    argument(
        name = "element",
        description = "Element expression. Can be a constant, column, or function, and any combination of array operators."
    ),
    argument(
        name = "count",
        description = "Value of how many times to repeat the element."
    )
)]
#[derive(Debug)]
pub struct ArrayRepeat {
    signature: Signature,
    aliases: Vec<String>,
}

impl Default for ArrayRepeat {
    fn default() -> Self {
        Self::new()
    }
}

impl ArrayRepeat {
    pub fn new() -> Self {
        Self {
            signature: Signature::user_defined(Volatility::Immutable),
            aliases: vec![String::from("list_repeat")],
        }
    }
}

impl ScalarUDFImpl for ArrayRepeat {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "array_repeat"
    }

    fn signature(&self) -> &Signature {
        &self.signature
    }

    fn return_type(&self, arg_types: &[DataType]) -> Result<DataType> {
        Ok(List(Arc::new(Field::new_list_field(
            arg_types[0].clone(),
            true,
        ))))
    }

    fn invoke_with_args(
        &self,
        args: datafusion_expr::ScalarFunctionArgs,
    ) -> Result<ColumnarValue> {
        make_scalar_function(array_repeat_inner)(&args.args)
    }

    fn aliases(&self) -> &[String] {
        &self.aliases
    }

    fn coerce_types(&self, arg_types: &[DataType]) -> Result<Vec<DataType>> {
        let [first_type, second_type] = take_function_args(self.name(), arg_types)?;

        // Coerce the second argument to Int64/UInt64 if it's a numeric type
        let second = match second_type {
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                DataType::Int64
            }
            DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
                DataType::UInt64
            }
            _ => return exec_err!("count must be an integer type"),
        };

        Ok(vec![first_type.clone(), second])
    }

    fn documentation(&self) -> Option<&Documentation> {
        self.doc()
    }
}

/// Array_repeat SQL function
pub fn array_repeat_inner(args: &[ArrayRef]) -> Result<ArrayRef> {
    let element = &args[0];
    let count_array = &args[1];

    let count_array = match count_array.data_type() {
        DataType::Int64 => &cast(count_array, &DataType::UInt64)?,
        DataType::UInt64 => count_array,
        _ => return exec_err!("count must be an integer type"),
    };

    let count_array = as_uint64_array(count_array)?;

    match element.data_type() {
        List(_) => {
            let list_array = as_list_array(element)?;
            general_list_repeat::<i32>(list_array, count_array)
        }
        LargeList(_) => {
            let list_array = as_large_list_array(element)?;
            general_list_repeat::<i64>(list_array, count_array)
        }
        _ => general_repeat::<i32>(element, count_array),
    }
}

/// For each element of `array[i]` repeat `count_array[i]` times.
///
/// Assumption for the input:
///     1. `count[i] >= 0`
///     2. `array.len() == count_array.len()`
///
/// For example,
/// ```text
/// array_repeat(
///     [1, 2, 3], [2, 0, 1] => [[1, 1], [], [3]]
/// )
/// ```
fn general_repeat<O: OffsetSizeTrait>(
    array: &ArrayRef,
    count_array: &UInt64Array,
) -> Result<ArrayRef> {
    let data_type = array.data_type();
    let mut new_values = vec![];

    let count_vec = count_array
        .values()
        .to_vec()
        .iter()
        .map(|x| *x as usize)
        .collect::<Vec<_>>();

    for (row_index, &count) in count_vec.iter().enumerate() {
        let repeated_array = if array.is_null(row_index) {
            new_null_array(data_type, count)
        } else {
            let original_data = array.to_data();
            let capacity = Capacities::Array(count);
            let mut mutable =
                MutableArrayData::with_capacities(vec![&original_data], false, capacity);

            for _ in 0..count {
                mutable.extend(0, row_index, row_index + 1);
            }

            let data = mutable.freeze();
            arrow::array::make_array(data)
        };
        new_values.push(repeated_array);
    }

    let new_values: Vec<_> = new_values.iter().map(|a| a.as_ref()).collect();
    let values = compute::concat(&new_values)?;

    Ok(Arc::new(GenericListArray::<O>::try_new(
        Arc::new(Field::new_list_field(data_type.to_owned(), true)),
        OffsetBuffer::from_lengths(count_vec),
        values,
        None,
    )?))
}

/// Handle List version of `general_repeat`
///
/// For each element of `list_array[i]` repeat `count_array[i]` times.
///
/// For example,
/// ```text
/// array_repeat(
///     [[1, 2, 3], [4, 5], [6]], [2, 0, 1] => [[[1, 2, 3], [1, 2, 3]], [], [[6]]]
/// )
/// ```
fn general_list_repeat<O: OffsetSizeTrait>(
    list_array: &GenericListArray<O>,
    count_array: &UInt64Array,
) -> Result<ArrayRef> {
    let data_type = list_array.data_type();
    let value_type = list_array.value_type();
    let mut new_values = vec![];

    let count_vec = count_array
        .values()
        .to_vec()
        .iter()
        .map(|x| *x as usize)
        .collect::<Vec<_>>();

    for (list_array_row, &count) in list_array.iter().zip(count_vec.iter()) {
        let list_arr = match list_array_row {
            Some(list_array_row) => {
                let original_data = list_array_row.to_data();
                let capacity = Capacities::Array(original_data.len() * count);
                let mut mutable = MutableArrayData::with_capacities(
                    vec![&original_data],
                    false,
                    capacity,
                );

                for _ in 0..count {
                    mutable.extend(0, 0, original_data.len());
                }

                let data = mutable.freeze();
                let repeated_array = arrow::array::make_array(data);

                let list_arr = GenericListArray::<O>::try_new(
                    Arc::new(Field::new_list_field(value_type.clone(), true)),
                    OffsetBuffer::<O>::from_lengths(vec![original_data.len(); count]),
                    repeated_array,
                    None,
                )?;
                Arc::new(list_arr) as ArrayRef
            }
            None => new_null_array(data_type, count),
        };
        new_values.push(list_arr);
    }

    let lengths = new_values.iter().map(|a| a.len()).collect::<Vec<_>>();
    let new_values: Vec<_> = new_values.iter().map(|a| a.as_ref()).collect();
    let values = compute::concat(&new_values)?;

    Ok(Arc::new(ListArray::try_new(
        Arc::new(Field::new_list_field(data_type.to_owned(), true)),
        OffsetBuffer::<i32>::from_lengths(lengths),
        values,
        None,
    )?))
}
