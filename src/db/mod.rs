pub mod mutations;
pub mod queries;
pub mod schema;

use cozo::DataValue;

pub(crate) fn dv_strs<I, S>(items: I) -> DataValue
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    DataValue::List(items.into_iter().map(|s| DataValue::from(s.into())).collect())
}

pub(crate) fn dv_ints(items: impl IntoIterator<Item = i64>) -> DataValue {
    DataValue::List(items.into_iter().map(DataValue::from).collect())
}

pub(crate) fn dv_rows(rows: Vec<Vec<DataValue>>) -> DataValue {
    DataValue::List(rows.into_iter().map(DataValue::List).collect())
}

pub(crate) fn as_strs(v: &DataValue) -> Vec<String> {
    v.get_slice().map(|s| s.iter().filter_map(|x| x.get_str().map(str::to_owned)).collect()).unwrap_or_default()
}

pub(crate) fn as_ints(v: &DataValue) -> Vec<i64> {
    v.get_slice().map(|s| s.iter().filter_map(DataValue::get_int).collect()).unwrap_or_default()
}

pub(crate) fn str_of(v: &DataValue) -> String {
    v.get_str().unwrap_or_default().to_owned()
}

pub(crate) fn dv_floats(v: &[f32]) -> DataValue {
    DataValue::List(v.iter().map(|x| DataValue::from(*x as f64)).collect())
}
