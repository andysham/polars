use super::hash_join::prepare_hashed_relation;
use crate::chunked_array::builder::PrimitiveChunkedBuilder;
use crate::prelude::*;
use arrow::array::{PrimitiveBuilder, StringBuilder};
use enum_dispatch::enum_dispatch;
use num::{Num, NumCast, ToPrimitive, Zero};
use rayon::prelude::*;
use std::hash::Hash;

fn groupby<T>(a: impl Iterator<Item = T>) -> Vec<(usize, Vec<usize>)>
where
    T: Hash + Eq + Copy,
{
    let hash_tbl = prepare_hashed_relation(a);

    hash_tbl
        .into_iter()
        .map(|(_, indexes)| {
            let first = unsafe { *indexes.get_unchecked(0) };
            (first, indexes)
        })
        .collect()
}

#[enum_dispatch(Series)]
trait IntoGroupTuples {
    fn group_tuples(&self) -> Vec<(usize, Vec<usize>)> {
        unimplemented!()
    }
}

impl<T> IntoGroupTuples for ChunkedArray<T>
where
    T: PolarsIntegerType,
    T::Native: Eq + Hash,
{
    fn group_tuples(&self) -> Vec<(usize, Vec<usize>)> {
        if let Ok(slice) = self.cont_slice() {
            groupby(slice.iter())
        } else {
            groupby(self.into_iter())
        }
    }
}
impl IntoGroupTuples for BooleanChunked {
    fn group_tuples(&self) -> Vec<(usize, Vec<usize>)> {
        if self.is_optimal_aligned() {
            groupby(self.into_no_null_iter())
        } else {
            groupby(self.into_iter())
        }
    }
}

impl IntoGroupTuples for Utf8Chunked {
    fn group_tuples(&self) -> Vec<(usize, Vec<usize>)> {
        if self.is_optimal_aligned() {
            groupby(self.into_no_null_iter())
        } else {
            groupby(self.into_iter())
        }
    }
}

impl IntoGroupTuples for Float64Chunked {}
impl IntoGroupTuples for Float32Chunked {}
impl IntoGroupTuples for LargeListChunked {}

impl DataFrame {
    /// Group DataFrame using a Series column.
    ///
    /// # Example
    ///
    /// ```
    /// use polars::prelude::*;
    /// fn groupby_sum(df: &DataFrame) -> Result<DataFrame> {
    ///     df.groupby("column_name")?
    ///     .select("agg_column_name")
    ///     .sum()
    /// }
    /// ```
    pub fn groupby(&self, by: &str) -> Result<GroupBy> {
        let groups = if let Some(s) = self.column(by) {
            s.group_tuples()
        } else {
            return Err(PolarsError::NotFound);
        };

        Ok(GroupBy {
            df: self,
            by: by.to_string(),
            groups,
            selection: None,
        })
    }
}

/// Returned by a groupby operation on a DataFrame. This struct supports
/// several aggregations.
///
/// Until described otherwise, the examples in this struct are performed on the following DataFrame:
///
/// ```rust
/// use polars::prelude::*;
///
/// let dates = &[
/// "2020-08-21",
/// "2020-08-21",
/// "2020-08-22",
/// "2020-08-23",
/// "2020-08-22",
/// ];
/// // date format
/// let fmt = "%Y-%m-%d";
/// // create date series
/// let s0 = Date32Chunked::parse_from_str_slice("date", dates, fmt)
///         .into_series();
/// // create temperature series
/// let s1 = Series::new("temp", [20, 10, 7, 9, 1].as_ref());
/// // create rain series
/// let s2 = Series::new("rain", [0.2, 0.1, 0.3, 0.1, 0.01].as_ref());
/// // create a new DataFrame
/// let df = DataFrame::new(vec![s0, s1, s2]).unwrap();
/// println!("{:?}", df);
/// ```
///
/// Outputs:
///
/// ```text
/// +------------+------+------+
/// | date       | temp | rain |
/// | ---        | ---  | ---  |
/// | date32     | i32  | f64  |
/// +============+======+======+
/// | 2020-08-21 | 20   | 0.2  |
/// +------------+------+------+
/// | 2020-08-21 | 10   | 0.1  |
/// +------------+------+------+
/// | 2020-08-22 | 7    | 0.3  |
/// +------------+------+------+
/// | 2020-08-23 | 9    | 0.1  |
/// +------------+------+------+
/// | 2020-08-22 | 1    | 0.01 |
/// +------------+------+------+
/// ```
///
#[derive(Debug, Clone)]
pub struct GroupBy<'a> {
    df: &'a DataFrame,
    /// By which column should the grouping operation be performed.
    pub by: String,
    // [first idx, [other idx]]
    groups: Vec<(usize, Vec<usize>)>,
    selection: Option<String>,
}

#[enum_dispatch(Series)]
trait NumericAggSync {
    fn agg_mean(&self, _groups: &Vec<(usize, Vec<usize>)>) -> Series {
        unimplemented!()
    }
    fn agg_min(&self, _groups: &Vec<(usize, Vec<usize>)>) -> Series {
        unimplemented!()
    }
    fn agg_max(&self, _groups: &Vec<(usize, Vec<usize>)>) -> Series {
        unimplemented!()
    }
    fn agg_sum(&self, _groups: &Vec<(usize, Vec<usize>)>) -> Series {
        unimplemented!()
    }
}

impl NumericAggSync for BooleanChunked {}
impl NumericAggSync for Utf8Chunked {}
impl NumericAggSync for LargeListChunked {}

impl<T> NumericAggSync for ChunkedArray<T>
where
    T: PolarsNumericType + Sync,
    T::Native: std::ops::Add<Output = T::Native> + Num + NumCast,
{
    fn agg_mean(&self, groups: &Vec<(usize, Vec<usize>)>) -> Series {
        Series::Float64(
            groups
                .par_iter()
                .map(|(_first, idx)| {
                    // Fast path
                    if let Ok(slice) = self.cont_slice() {
                        let mut sum = 0.;
                        for i in idx {
                            sum = sum + slice[*i].to_f64().unwrap()
                        }
                        Some(sum / idx.len() as f64)
                    } else {
                        let take = unsafe {
                            self.take_unchecked(idx.into_iter().copied(), Some(self.len()))
                        };
                        let opt_sum: Option<T::Native> = take.sum();
                        opt_sum.map(|sum| sum.to_f64().unwrap() / idx.len() as f64)
                    }
                })
                .collect(),
        )
    }

    fn agg_min(&self, groups: &Vec<(usize, Vec<usize>)>) -> Series {
        groups
            .par_iter()
            .map(|(_first, idx)| {
                if let Ok(slice) = self.cont_slice() {
                    let mut min = None;
                    for i in idx {
                        let v = slice[*i];

                        min = match min {
                            Some(min) => {
                                if min < v {
                                    Some(min)
                                } else {
                                    Some(v)
                                }
                            }
                            None => Some(v),
                        };
                    }
                    min
                } else {
                    let take =
                        unsafe { self.take_unchecked(idx.into_iter().copied(), Some(self.len())) };
                    take.min()
                }
            })
            .collect::<ChunkedArray<T>>()
            .into_series()
    }

    fn agg_max(&self, groups: &Vec<(usize, Vec<usize>)>) -> Series {
        groups
            .par_iter()
            .map(|(_first, idx)| {
                if let Ok(slice) = self.cont_slice() {
                    let mut max = None;
                    for i in idx {
                        let v = slice[*i];

                        max = match max {
                            Some(max) => {
                                if max > v {
                                    Some(max)
                                } else {
                                    Some(v)
                                }
                            }
                            None => Some(v),
                        };
                    }
                    max
                } else {
                    let take =
                        unsafe { self.take_unchecked(idx.into_iter().copied(), Some(self.len())) };
                    take.max()
                }
            })
            .collect::<ChunkedArray<T>>()
            .into_series()
    }

    fn agg_sum(&self, groups: &Vec<(usize, Vec<usize>)>) -> Series {
        groups
            .par_iter()
            .map(|(_first, idx)| {
                if let Ok(slice) = self.cont_slice() {
                    let mut sum = Zero::zero();
                    for i in idx {
                        sum = sum + slice[*i]
                    }
                    Some(sum)
                } else {
                    let take =
                        unsafe { self.take_unchecked(idx.into_iter().copied(), Some(self.len())) };
                    take.sum()
                }
            })
            .collect::<ChunkedArray<T>>()
            .into_series()
    }
}

macro_rules! impl_list_groupby_macro {
    ($obj:ident, $gb_macro:ident, $gb_macro_utf8:ident) => {{
        match $obj.dtype() {
            ArrowDataType::Utf8 => $gb_macro_utf8!(),
            ArrowDataType::Boolean => $gb_macro!(BooleanType),
            ArrowDataType::UInt8 => $gb_macro!(UInt8Type),
            ArrowDataType::UInt16 => $gb_macro!(UInt16Type),
            ArrowDataType::UInt32 => $gb_macro!(UInt32Type),
            ArrowDataType::UInt64 => $gb_macro!(UInt64Type),
            ArrowDataType::Int8 => $gb_macro!(Int8Type),
            ArrowDataType::Int16 => $gb_macro!(Int16Type),
            ArrowDataType::Int32 => $gb_macro!(Int32Type),
            ArrowDataType::Int64 => $gb_macro!(Int64Type),
            ArrowDataType::Float32 => $gb_macro!(Float32Type),
            ArrowDataType::Float64 => $gb_macro!(Float64Type),
            ArrowDataType::Date32(DateUnit::Day) => $gb_macro!(Date32Type),
            ArrowDataType::Date64(DateUnit::Millisecond) => $gb_macro!(Date64Type),
            ArrowDataType::Time32(TimeUnit::Millisecond) => $gb_macro!(Time32MillisecondType),
            ArrowDataType::Time32(TimeUnit::Second) => $gb_macro!(Time32SecondType),
            ArrowDataType::Time64(TimeUnit::Nanosecond) => $gb_macro!(Time64NanosecondType),
            ArrowDataType::Time64(TimeUnit::Microsecond) => $gb_macro!(Time64MicrosecondType),
            ArrowDataType::Interval(IntervalUnit::DayTime) => $gb_macro!(IntervalDayTimeType),
            ArrowDataType::Interval(IntervalUnit::YearMonth) => $gb_macro!(IntervalYearMonthType),
            ArrowDataType::Duration(TimeUnit::Nanosecond) => $gb_macro!(DurationNanosecondType),
            ArrowDataType::Duration(TimeUnit::Microsecond) => $gb_macro!(DurationMicrosecondType),
            ArrowDataType::Duration(TimeUnit::Millisecond) => $gb_macro!(DurationMillisecondType),
            ArrowDataType::Duration(TimeUnit::Second) => $gb_macro!(DurationSecondType),
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, _) => {
                $gb_macro!(TimestampNanosecondType)
            }
            ArrowDataType::Timestamp(TimeUnit::Microsecond, _) => {
                $gb_macro!(TimestampMicrosecondType)
            }
            ArrowDataType::Timestamp(TimeUnit::Millisecond, _) => $gb_macro!(Time32MillisecondType),
            ArrowDataType::Timestamp(TimeUnit::Second, _) => $gb_macro!(TimestampSecondType),
            _ => unimplemented!(),
        }
    }};
}

impl<'a> GroupBy<'a> {
    /// Select the column by which the determine the groups.
    pub fn select(mut self, name: &str) -> Self {
        self.selection = Some(name.to_string());
        self
    }

    fn keys(&self) -> Series {
        unsafe {
            self.df.f_column(&self.by).take_iter_unchecked(
                self.groups.iter().map(|(idx, _)| *idx),
                Some(self.groups.len()),
            )
        }
    }

    fn prepare_agg(&self) -> Result<(&String, Series, &Series)> {
        let name = match &self.selection {
            Some(name) => name,
            None => return Err(PolarsError::NoSelection),
        };

        let keys = self.keys();
        let agg_col = self.df.column(name).ok_or(PolarsError::NotFound)?;
        Ok((name, keys, agg_col))
    }

    /// Aggregate grouped series and compute the mean per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars::prelude::*;
    /// fn example(df: DataFrame) -> Result<DataFrame> {
    ///     df.groupby("date")?.select("temp").mean()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+-----------+
    /// | date       | temp_mean |
    /// | ---        | ---       |
    /// | date32     | f64       |
    /// +============+===========+
    /// | 2020-08-23 | 9         |
    /// +------------+-----------+
    /// | 2020-08-22 | 4         |
    /// +------------+-----------+
    /// | 2020-08-21 | 15        |
    /// +------------+-----------+
    /// ```
    pub fn mean(&self) -> Result<DataFrame> {
        let (name, keys, agg_col) = self.prepare_agg()?;
        let new_name = format!["{}_mean", name];

        let mut agg = agg_col.agg_mean(&self.groups);
        agg.rename(&new_name);
        DataFrame::new(vec![keys, agg])
    }

    /// Aggregate grouped series and compute the sum per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars::prelude::*;
    /// fn example(df: DataFrame) -> Result<DataFrame> {
    ///     df.groupby("date")?.select("temp").sum()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+----------+
    /// | date       | temp_sum |
    /// | ---        | ---      |
    /// | date32     | i32      |
    /// +============+==========+
    /// | 2020-08-23 | 9        |
    /// +------------+----------+
    /// | 2020-08-22 | 8        |
    /// +------------+----------+
    /// | 2020-08-21 | 30       |
    /// +------------+----------+
    /// ```
    pub fn sum(&self) -> Result<DataFrame> {
        let (name, keys, agg_col) = self.prepare_agg()?;
        let new_name = format!["{}_sum", name];
        let mut agg = agg_col.agg_sum(&self.groups);
        agg.rename(&new_name);
        DataFrame::new(vec![keys, agg])
    }

    /// Aggregate grouped series and compute the minimal value per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars::prelude::*;
    /// fn example(df: DataFrame) -> Result<DataFrame> {
    ///     df.groupby("date")?.select("temp").min()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+----------+
    /// | date       | temp_min |
    /// | ---        | ---      |
    /// | date32     | i32      |
    /// +============+==========+
    /// | 2020-08-23 | 9        |
    /// +------------+----------+
    /// | 2020-08-22 | 1        |
    /// +------------+----------+
    /// | 2020-08-21 | 10       |
    /// +------------+----------+
    /// ```
    pub fn min(&self) -> Result<DataFrame> {
        let (name, keys, agg_col) = self.prepare_agg()?;
        let new_name = format!["{}_min", name];
        let mut agg = apply_method_numeric_series!(agg_col, agg_min, &self.groups);
        agg.rename(&new_name);
        DataFrame::new(vec![keys, agg])
    }

    /// Aggregate grouped series and compute the maximum value per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars::prelude::*;
    /// fn example(df: DataFrame) -> Result<DataFrame> {
    ///     df.groupby("date")?.select("temp").max()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+----------+
    /// | date       | temp_max |
    /// | ---        | ---      |
    /// | date32     | i32      |
    /// +============+==========+
    /// | 2020-08-23 | 9        |
    /// +------------+----------+
    /// | 2020-08-22 | 7        |
    /// +------------+----------+
    /// | 2020-08-21 | 20       |
    /// +------------+----------+
    /// ```
    pub fn max(&self) -> Result<DataFrame> {
        let (name, keys, agg_col) = self.prepare_agg()?;
        let new_name = format!["{}_max", name];
        let mut agg = agg_col.agg_max(&self.groups);
        agg.rename(&new_name);
        DataFrame::new(vec![keys, agg])
    }

    /// Aggregate grouped series and compute the number of values per group.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars::prelude::*;
    /// fn example(df: DataFrame) -> Result<DataFrame> {
    ///     df.groupby("date")?.select("temp").count()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    /// +------------+------------+
    /// | date       | temp_count |
    /// | ---        | ---        |
    /// | date32     | u32        |
    /// +============+============+
    /// | 2020-08-23 | 1          |
    /// +------------+------------+
    /// | 2020-08-22 | 2          |
    /// +------------+------------+
    /// | 2020-08-21 | 2          |
    /// +------------+------------+
    /// ```
    pub fn count(&self) -> Result<DataFrame> {
        let (name, keys, agg_col) = self.prepare_agg()?;
        let new_name = format!["{}_count", name];

        let mut builder = PrimitiveChunkedBuilder::new(&new_name, self.groups.len());
        for (_first, idx) in &self.groups {
            let s =
                unsafe { agg_col.take_iter_unchecked(idx.into_iter().copied(), Some(idx.len())) };
            builder.append_value(s.len() as u32);
        }
        let ca = builder.finish();
        let agg = Series::UInt32(ca);
        DataFrame::new(vec![keys, agg])
    }

    /// Aggregate the groups of the groupby operation into lists.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use polars::prelude::*;
    /// fn example(df: DataFrame) -> Result<DataFrame> {
    ///     // GroupBy and aggregate to Lists
    ///     df.groupby("date")?.select("temp").agg_list()
    /// }
    /// ```
    /// Returns:
    ///
    /// ```text
    ///  +------------+---------------+
    ///  | date       | temp_agg_list |
    ///  | ---        | ---           |
    ///  | date32     | list [i32]    |
    ///  +============+===============+
    ///  | 2020-08-23 | list [Int32]  |
    ///  +------------+---------------+
    ///  | 2020-08-22 | list [Int32]  |
    ///  +------------+---------------+
    ///  | 2020-08-21 | list [Int32]  |
    ///  +------------+---------------+
    /// ```
    pub fn agg_list(&self) -> Result<DataFrame> {
        let (name, keys, agg_col) = self.prepare_agg()?;
        let new_name = format!["{}_agg_list", name];

        macro_rules! impl_gb {
            ($type:ty) => {{
                let values_builder = PrimitiveBuilder::<$type>::new(self.groups.len());
                let mut builder = LargeListPrimitiveChunkedBuilder::new(
                    &new_name,
                    values_builder,
                    self.groups.len(),
                );
                for (_first, idx) in &self.groups {
                    let s = unsafe {
                        agg_col.take_iter_unchecked(idx.into_iter().copied(), Some(idx.len()))
                    };
                    builder.append_opt_series(Some(&s))
                }
                let list = builder.finish().into_series();
                DataFrame::new(vec![keys, list])
            }};
        }

        macro_rules! impl_gb_utf8 {
            () => {{
                let values_builder = StringBuilder::new(self.groups.len());
                let mut builder =
                    LargeListUtf8ChunkedBuilder::new(&new_name, values_builder, self.groups.len());
                for (_first, idx) in &self.groups {
                    let s = unsafe {
                        agg_col.take_iter_unchecked(idx.into_iter().copied(), Some(idx.len()))
                    };
                    builder.append_opt_series(Some(&s))
                }
                let list = builder.finish().into_series();
                DataFrame::new(vec![keys, list])
            }};
        }
        impl_list_groupby_macro!(agg_col, impl_gb, impl_gb_utf8)
    }
}

#[cfg(test)]
mod test {
    use crate::prelude::*;

    #[test]
    fn test_group_by() {
        let s0 = Date32Chunked::parse_from_str_slice(
            "date",
            &[
                "2020-08-21",
                "2020-08-21",
                "2020-08-22",
                "2020-08-23",
                "2020-08-22",
            ],
            "%Y-%m-%d",
        )
        .into_series();
        let s1 = Series::new("temp", [20, 10, 7, 9, 1].as_ref());
        let s2 = Series::new("rain", [0.2, 0.1, 0.3, 0.1, 0.01].as_ref());
        let df = DataFrame::new(vec![s0, s1, s2]).unwrap();
        println!("{:?}", df);

        println!(
            "{:?}",
            df.groupby("date").unwrap().select("temp").count().unwrap()
        );
        println!(
            "{:?}",
            df.groupby("date").unwrap().select("temp").mean().unwrap()
        );
        println!(
            "{:?}",
            df.groupby("date").unwrap().select("temp").sum().unwrap()
        );
        println!(
            "{:?}",
            df.groupby("date").unwrap().select("temp").min().unwrap()
        );
        println!(
            "{:?}",
            df.groupby("date").unwrap().select("temp").max().unwrap()
        );
        println!(
            "{:?}",
            df.groupby("date")
                .unwrap()
                .select("temp")
                .agg_list()
                .unwrap()
        );
    }
}
