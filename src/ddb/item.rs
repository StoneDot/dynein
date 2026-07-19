/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License").
 * You may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use aws_sdk_dynamodb::types::AttributeValue;
use std::collections::HashMap;
use thiserror::Error;

///
/// Calculates the estimated size of a DynamoDB item.
///
/// This function estimates the size of a DynamoDB item by summing up the size of its keys and the
/// size of their corresponding attribute values. The size of each attribute value is determined
/// using the `calculate_estimated_attr_size` function.
///
/// # Arguments
///
/// * `item` - A reference to a `HashMap` where the keys are `String` and the values are `AttributeValue`.
///
/// # Returns
///
/// This function returns a `Result` containing the total estimated size of the item in bytes. If
/// the item contains an invalid attribute, an `ItemSizeCalculationError` is returned.
///
/// # Errors
///
/// Returns an `ItemSizeCalculationError` if any attribute value in the item is invalid.
pub fn calculate_estimated_item_size(
    item: &HashMap<String, AttributeValue>,
) -> Result<usize, ItemSizeCalculationError> {
    let mut total = 0;
    for (k, v) in item {
        total += k.len() + calculate_estimated_attr_size(v)?;
    }
    Ok(total)
}

///
/// Enum representing possible errors during item size calculation.
/// This enum derives the `Error`, `PartialEq`, `Debug`, `Clone`, and `Hash` traits.
///
/// # Variants
///
/// * `InvalidAttribute` - Represents an error when an invalid attribute is encountered.
/// * `InvalidNumberFormat` - Represents an error when a number attribute has an invalid format.
///
#[derive(Error, PartialEq, Debug, Clone, Hash)]
pub enum ItemSizeCalculationError {
    #[error("contain an invalid attribute")]
    InvalidAttribute,
    #[error("contain an invalid number attribute")]
    InvalidNumberFormat(#[from] NumberParseError),
}

/// Calculate the estimated size of an `AttributeValue`.
///
/// A size of attribute value is determined by heuristic described in the
/// [official document](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/CapacityUnitCalculations.html).
///
/// # Arguments
///
/// * `attr` - The attribute value to calculate the size for.
///
/// # Returns
///
/// The estimated size of the attribute value, or an `ItemSizeCalculationError` if the attribute value is invalid.
///
/// # Errors
///
/// Returns an `ItemSizeCalculationError` if the attribute value is invalid.
pub fn calculate_estimated_attr_size(
    attr: &AttributeValue,
) -> Result<usize, ItemSizeCalculationError> {
    match attr {
        AttributeValue::B(blob) => Ok(blob.as_ref().len()),
        AttributeValue::Bool(_) => Ok(1),
        AttributeValue::Bs(vec) => Ok(vec.iter().map(|blob| blob.as_ref().len()).sum::<usize>()),
        AttributeValue::L(vec) => Ok(vec
            .iter()
            .map(|x| calculate_estimated_attr_size(x).map(|s| s + 1))
            .sum::<Result<usize, ItemSizeCalculationError>>()
            .map(|x| x + 3)?),
        AttributeValue::M(vec) => Ok(vec
            .iter()
            .map(|x| calculate_estimated_attr_size(x.1).map(|s| s + x.0.len() + 1))
            .sum::<Result<usize, ItemSizeCalculationError>>()
            .map(|x| x + 3)?),
        AttributeValue::N(num) => Ok(calc_num_size(num)?),
        AttributeValue::Ns(vec) => Ok(vec
            .iter()
            .map(|num| calc_num_size(num))
            .sum::<Result<usize, NumberParseError>>()?),
        AttributeValue::Null(_) => Ok(1),
        AttributeValue::S(str) => Ok(str.len()),
        AttributeValue::Ss(vec) => Ok(vec.iter().map(|str| str.len()).sum::<usize>()),
        _ => Err(ItemSizeCalculationError::InvalidAttribute),
    }
}

fn calc_num_size(num: &str) -> Result<usize, NumberParseError> {
    let (frac, _exp) = calc_digits(num.as_bytes())?;
    Ok((frac as usize).div_ceil(2) + 1)
}

///
/// Enum to represent possible errors that can occur when parsing a string into a number.
///
/// Variants:
///
/// * `UnexpectedChar { unexpected_byte, pos }` - Occurs when an unexpected byte is encountered at a specific position.
/// * `IncompleteInput` - Occurs when the provided string is incomplete for parsing a valid number.
/// * `EmptyString` - Occurs when an empty string is provided.
///
/// This enum derives the `Error`, `PartialEq`, `Debug`, and `Clone` traits for better error handling and debugging.
#[derive(Error, PartialEq, Debug, Clone, Hash)]
pub enum NumberParseError {
    #[error("unexpected byte '{unexpected_byte:?}' at {pos:?}")]
    UnexpectedChar { unexpected_byte: u8, pos: usize },
    #[error("provided string is incomplete as a number")]
    IncompleteInput,
    #[error("empty string is provided")]
    EmptyString,
}

/// Calculates the number of significant digits and exponent of a given byte string.
///
/// # Arguments
///
/// * `str` - The byte string to calculate the digits from.
///
/// # Returns
///
/// Returns a tuple `(frac, exp)` where `frac` is the number of fraction digits and `exp` is the exponent.
/// If an error occurs during parsing, a `NumberParseError` is returned.
///
/// # Examples
///
/// ```
/// assert_eq!(calc_digits(b"123.456"), Ok((6, 2)));
/// assert_eq!(calc_digits(b"0.0012300"), Ok((3, -3)));
/// assert_eq!(calc_digits(b"-12.34"), Ok((4, 1)));
/// assert_eq!(calc_digits(b"+.1"), Ok((1, -1)));
/// assert_eq!(calc_digits(b"-0."), Err(NumberParseError::IncompleteInput));
/// ```
fn calc_digits(str: &[u8]) -> Result<(i32, i32), NumberParseError> {
    let mut pos: usize = 0;
    let mut frac = 0;
    let mut exp = 0;
    let mut zeros = 0;
    let mut occur_significant = false;

    // Handle empty string
    if str.is_empty() {
        return Err(NumberParseError::EmptyString);
    }

    // Handle sign
    if str[pos] == b'+' || str[pos] == b'-' {
        pos += 1;
    }
    if pos == str.len() {
        return Err(NumberParseError::IncompleteInput);
    }

    // Handle before the decimal point
    while pos < str.len() {
        if str[pos] == b'.' {
            break;
        }
        if !str[pos].is_ascii_digit() {
            return Err(NumberParseError::UnexpectedChar {
                pos,
                unexpected_byte: str[pos],
            });
        }

        if occur_significant {
            exp += 1;
        }
        if str[pos] == b'0' {
            if occur_significant {
                zeros += 1;
            }
        } else {
            frac += zeros + 1;
            zeros = 0;
            occur_significant = true;
        }
        pos += 1;
    }

    if pos < str.len() {
        // handle dot (.)
        if str[pos] == b'.' {
            pos += 1;
        } else {
            return Err(NumberParseError::UnexpectedChar {
                pos,
                unexpected_byte: str[pos],
            });
        }
    } else {
        // End the number
        return Ok((frac, exp));
    }

    // Unnecessary dot
    if pos == str.len() {
        return Err(NumberParseError::IncompleteInput);
    }

    // First zero must be ignored in case of `0.xxx`
    if !occur_significant {
        zeros = 0;
    }

    // Handle after the decimal point
    while pos < str.len() {
        if !str[pos].is_ascii_digit() {
            return Err(NumberParseError::UnexpectedChar {
                pos,
                unexpected_byte: str[pos],
            });
        }

        if !occur_significant {
            exp -= 1;
        }
        if str[pos] != b'0' {
            frac += zeros + 1;
            zeros = 0;
            occur_significant = true;
        } else if occur_significant {
            zeros += 1;
        }
        pos += 1;
    }

    Ok((frac, exp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calc_digits() {
        assert_eq!(calc_digits(b"0"), Ok((0, 0)));
        for d in [b"1", b"2", b"3", b"4", b"5", b"6", b"7", b"8", b"9"] {
            assert_eq!(calc_digits(d), Ok((1, 0)));
        }
        assert_eq!(calc_digits(b"9"), Ok((1, 0)));
        assert_eq!(calc_digits(b"10"), Ok((1, 1)));
        assert_eq!(calc_digits(b"11"), Ok((2, 1)));
        assert_eq!(calc_digits(b"99"), Ok((2, 1)));
        assert_eq!(calc_digits(b"100"), Ok((1, 2)));
        assert_eq!(calc_digits(b"101"), Ok((3, 2)));
        assert_eq!(calc_digits(b"110"), Ok((2, 2)));
        assert_eq!(calc_digits(b"0.1"), Ok((1, -1)));

        assert_eq!(calc_digits(b"10100"), Ok((3, 4)));
        assert_eq!(calc_digits(b"1010"), Ok((3, 3)));
        assert_eq!(calc_digits(b"101"), Ok((3, 2)));
        assert_eq!(calc_digits(b"10.1"), Ok((3, 1)));
        assert_eq!(calc_digits(b"1.01"), Ok((3, 0)));
        assert_eq!(calc_digits(b"0.101"), Ok((3, -1)));
        assert_eq!(calc_digits(b"0.0101"), Ok((3, -2)));
        assert_eq!(calc_digits(b"0.00101"), Ok((3, -3)));

        assert_eq!(calc_digits(b"-23400"), Ok((3, 4)));
        assert_eq!(calc_digits(b"-2340"), Ok((3, 3)));
        assert_eq!(calc_digits(b"-234"), Ok((3, 2)));
        assert_eq!(calc_digits(b"-23.4"), Ok((3, 1)));
        assert_eq!(calc_digits(b"-2.34"), Ok((3, 0)));
        assert_eq!(calc_digits(b"-0.234"), Ok((3, -1)));
        assert_eq!(calc_digits(b"-0.0234"), Ok((3, -2)));
        assert_eq!(calc_digits(b"-0.00234"), Ok((3, -3)));

        assert_eq!(calc_digits(b"+56700"), Ok((3, 4)));
        assert_eq!(calc_digits(b"+5670"), Ok((3, 3)));
        assert_eq!(calc_digits(b"+567"), Ok((3, 2)));
        assert_eq!(calc_digits(b"+56.7"), Ok((3, 1)));
        assert_eq!(calc_digits(b"+5.67"), Ok((3, 0)));
        assert_eq!(calc_digits(b"+0.567"), Ok((3, -1)));
        assert_eq!(calc_digits(b"+0.0567"), Ok((3, -2)));
        assert_eq!(calc_digits(b"+0.00567"), Ok((3, -3)));

        assert_eq!(calc_digits(b"-0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"),
                   Ok((1, -130)));
        assert_eq!(calc_digits(b"-999999999999999999999999999999999999990000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"),
                   Ok((38, 125)));
        assert_eq!(calc_digits(b"0.0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"),
                   Ok((1, -130)));
        assert_eq!(calc_digits(b"999999999999999999999999999999999999990000000000000000000000000000000000000000000000000000000000000000000000000000000000000000"),
                   Ok((38, 125)));

        assert_eq!(calc_digits(b".8"), Ok((1, -1)));
        assert_eq!(calc_digits(b"-.8"), Ok((1, -1)));
        assert_eq!(calc_digits(b"+.8"), Ok((1, -1)));
        assert_eq!(calc_digits(b"+.08"), Ok((1, -2)));
        assert_eq!(calc_digits(b"-.08"), Ok((1, -2)));

        assert_eq!(calc_digits(b"10.00"), Ok((1, 1)));
        assert_eq!(calc_digits(b"10.0"), Ok((1, 1)));
        assert_eq!(calc_digits(b"1.0"), Ok((1, 0)));
        assert_eq!(calc_digits(b"0.10"), Ok((1, -1)));
        assert_eq!(calc_digits(b"0.100"), Ok((1, -1)));

        assert_eq!(calc_digits(b"01"), Ok((1, 0)));
        assert_eq!(calc_digits(b"001"), Ok((1, 0)));
        assert_eq!(calc_digits(b"0010"), Ok((1, 1)));
        assert_eq!(calc_digits(b"00110"), Ok((2, 2)));
        assert_eq!(calc_digits(b"001100"), Ok((2, 3)));
        assert_eq!(calc_digits(b"00"), Ok((0, 0)));
        assert_eq!(calc_digits(b"000"), Ok((0, 0)));

        assert_eq!(calc_digits(b"0."), Err(NumberParseError::IncompleteInput));
        assert_eq!(calc_digits(b"1."), Err(NumberParseError::IncompleteInput));
        assert_eq!(calc_digits(b"."), Err(NumberParseError::IncompleteInput));
        assert_eq!(calc_digits(b"+"), Err(NumberParseError::IncompleteInput));
        assert_eq!(calc_digits(b"-"), Err(NumberParseError::IncompleteInput));
        assert_eq!(calc_digits(b""), Err(NumberParseError::EmptyString));
        assert_eq!(
            calc_digits(b"a"),
            Err(NumberParseError::UnexpectedChar {
                pos: 0,
                unexpected_byte: b'a'
            })
        );
        assert_eq!(
            calc_digits(b"0e3"),
            Err(NumberParseError::UnexpectedChar {
                pos: 1,
                unexpected_byte: b'e'
            })
        );

        assert_eq!(calc_digits(b"123.456"), Ok((6, 2)));
        assert_eq!(calc_digits(b"0.0012300"), Ok((3, -3)));
        assert_eq!(calc_digits(b"-12.34"), Ok((4, 1)));
        assert_eq!(calc_digits(b"+.1"), Ok((1, -1)));
        assert_eq!(calc_digits(b"-0."), Err(NumberParseError::IncompleteInput));
        assert_eq!(
            calc_digits(b"abc"),
            Err(NumberParseError::UnexpectedChar {
                pos: 0,
                unexpected_byte: 97
            })
        );
    }

    #[test]
    fn test_attr_size_scalar_types() {
        use aws_smithy_types::Blob;
        // String: UTF-8 byte length ("あ" is 3 bytes).
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::S("abc".into())),
            Ok(3)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::S("aあ".into())),
            Ok(4)
        );
        // Number: ceil(significant digits / 2) + 1.
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::N("0".into())),
            Ok(1)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::N("1".into())),
            Ok(2)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::N("99".into())),
            Ok(2)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::N("101".into())),
            Ok(3)
        );
        // Bool and Null occupy one byte each.
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::Bool(true)),
            Ok(1)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::Null(true)),
            Ok(1)
        );
        // Binary: raw byte length.
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::B(Blob::new(b"xyz".to_vec()))),
            Ok(3)
        );
    }

    #[test]
    fn test_attr_size_sets_sum_their_elements() {
        use aws_smithy_types::Blob;
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::Ss(vec!["ab".into(), "c".into()])),
            Ok(3)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::Ns(vec!["1".into(), "99".into()])),
            Ok(4)
        );
        assert_eq!(
            calculate_estimated_attr_size(&AttributeValue::Bs(vec![
                Blob::new(b"ab".to_vec()),
                Blob::new(b"c".to_vec()),
            ])),
            Ok(3)
        );
    }

    #[test]
    fn test_attr_size_list_and_map_have_container_overhead() {
        // List: 3 bytes + (element size + 1) per element.
        let list = AttributeValue::L(vec![
            AttributeValue::S("ab".into()),
            AttributeValue::Bool(true),
        ]);
        assert_eq!(
            calculate_estimated_attr_size(&list),
            Ok(3 + (2 + 1) + (1 + 1))
        );
        // Map: 3 bytes + (key length + value size + 1) per entry.
        let map = AttributeValue::M(HashMap::from([(
            "k".to_string(),
            AttributeValue::S("ab".into()),
        )]));
        assert_eq!(calculate_estimated_attr_size(&map), Ok(3 + (1 + 2 + 1)));
    }

    #[test]
    fn test_item_size_sums_attribute_names_and_values() {
        let item = HashMap::from([
            ("pk".to_string(), AttributeValue::S("ab".into())),
            ("n".to_string(), AttributeValue::N("1".into())),
        ]);
        assert_eq!(calculate_estimated_item_size(&item), Ok((2 + 2) + (1 + 2)));
    }

    #[test]
    fn test_attr_size_invalid_number_is_an_error() {
        assert!(matches!(
            calculate_estimated_attr_size(&AttributeValue::N("abc".into())),
            Err(ItemSizeCalculationError::InvalidNumberFormat(_))
        ));
    }
}
