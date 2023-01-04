use polars::prelude::*;
use pyo3::prelude::*;
use std::hash::Hash;
use std::ops::AddAssign;
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

enum Distribution {
    TimingDistribution,
    MemoryDistribution,
    CustomDistributionExponential,
    CustomDistributionLinear,
}

impl FromStr for Distribution {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "timing_distribution" => Ok(Distribution::TimingDistribution),
            "memory_distribution" => Ok(Distribution::MemoryDistribution),
            "custom_distribution_exponential" => Ok(Distribution::CustomDistributionExponential),
            "custom_distribution_linear" => Ok(Distribution::CustomDistributionLinear),
            _ => Err("valid distribution type not provided"),
        }
    }
}

fn normalize_histogram_glam(hist: HashMap<i64, i64>) -> Vec<(usize, f64)> {
    // non-concurrent normalization, because GLAM aggregation is partitioned
    // and the concurrency makes more sense on those partitions.
    let total = hist.values().sum::<i64>() as f64;

    hist.iter()
        .map(|(k, v)| (*k as usize, *v as f64 / total))
        .collect()
}

fn map_sum_generic<T: Hash + Eq + Copy, U: AddAssign + Copy>(
    keys: Vec<T>,
    values: Vec<U>,
) -> HashMap<T, U> {
    // generic version of map sum, cannot be exposed to python easily
    let mut result_map = HashMap::new();

    for (k, v) in keys.iter().zip(values.iter()) {
        result_map.entry(*k).and_modify(|y| *y += *v).or_insert(*v);
    }

    result_map
}

fn float_map_sum_tuples(r: Vec<(usize, f64)>) -> HashMap<usize, f64> {
    // non-concurrent map sum specifically for glam transformations
    let mut result_map = HashMap::new();

    r.iter().for_each(|x| {
        result_map
            .entry(x.0)
            .and_modify(|y| *y += x.1)
            .or_insert(x.1);
    });

    result_map
}

fn sample_to_bucket_idx(sample: f64, log_base: f64, buckets_per_magnitude: f64) -> usize {
    let exponent = f64::powf(log_base, 1f64 / buckets_per_magnitude);
    f64::ceil((sample + 1.0).ln() / exponent.ln()) as usize
}

fn generate_functional_buckets(
    log_base: usize,
    buckets_per_magnitude: usize,
    range_max: usize,
) -> Vec<usize> {
    let mut buckets = HashSet::new();
    buckets.insert(0);

    let max_bucket_id = sample_to_bucket_idx(
        range_max as f64,
        log_base as f64,
        buckets_per_magnitude as f64,
    );

    for idx in 0..max_bucket_id {
        let bucket =
            f64::powf(log_base as f64, idx as f64 / buckets_per_magnitude as f64).floor() as usize;
        if bucket < range_max {
            buckets.insert(bucket);
        } else {
            break;
        }
    }

    Vec::from_iter(buckets)
}

fn generate_exponential_buckets(
    min_bucket: usize,
    max_bucket: usize,
    n_buckets: usize,
) -> Vec<usize> {
    let log_max = f64::ln(max_bucket as f64);
    let mut current = min_bucket.max(1);

    let mut out_array = vec![0, current];
    let stop = (n_buckets.min(10_000)).min(max_bucket);

    for bucket_idx in 2..stop {
        let log_current = f64::ln(current as f64);
        let log_next = ((log_max - log_current) / ((n_buckets - bucket_idx) as f64)) + log_current;
        let next_value = f64::round(f64::exp(log_next)) as usize;
        if next_value > current {
            out_array.push(next_value);
            current = next_value;
        } else {
            out_array.push(current + 1);
            current += 1;
        }
    }

    out_array
}

fn count_users(hist: &mut HashMap<usize, f64>) -> usize {
    // this is a neat trick; because all of the client histograms sum to
    // one, you can just sum the values to get N_reporting
    hist.values().sum::<f64>() as usize
}

fn transform_to_dirichlet_estimator(hist: &mut HashMap<usize, f64>, n_reporting: usize) {
    // Operates on a single histogram.
    // The histogram is filled at this point, can take K from length (total buckets)
    // -- Dirichlet distribution density for each bucket in a histogram.
    //  -- Given {k1: p1,k2:p2} where p’s are proportions(and p1, p2 sum to 1)
    //  -- return {k1: (P1+1/K) / (nreporting+1), k2:(P2+1/K) / (nreporting+1)}

    let k = hist.len() as f64;
    let n_reporting = n_reporting as f64;

    hist.iter_mut()
        .for_each(|(_, v)| *v = (*v + 1.0f64 / k) / n_reporting);
}

fn fill_buckets(hist: &mut HashMap<usize, f64>, buckets: &Vec<usize>) {
    buckets.iter().for_each(|x| {
        hist.entry(*x).or_insert(0f64);
    });
}

fn calculate_dirichlet_distribution(
    histogram_vector: HashMap<usize, f64>,
    histogram_type: &str,
    n_buckets: usize,
) -> Result<HashMap<usize, f64>, String> {
    // 1. aggregate client level histograms <- per client level
    // client_id || [bucket: sum, bucket: sum...]
    //
    // 2. normalize such that sum(values) = 1
    // client_id || [bucket: sum/total, bucket: sum/total...]
    //
    // 3. add all the normalized values together such that each bucket now sums
    // all of the normalized per-client values
    // [bucket: [u1_normalized, u2_normalized, ...]
    //
    // 4. the total sum of all values accross all buckets  equals the number reporting,
    // a handy coincidence of normalizing at the per client level
    //
    // 5. for every bucket that has a value, calculate the dirichelt approx transformation
    // using N_reporting from 4
    //
    // 6.generate the array of all bucket values we need to fill in and
    // add the dirichlet transfromed value (5) to the appropriate bucket
    //

    let mut hist = histogram_vector; // when we take the dictionary in from Python,
                                     // we probably cannot take it as a reference

    // assuming at this point I have aggregated, normalized histograms
    let n_reporting = count_users(&mut hist);
    let range_max = hist.keys().max().unwrap().clone(); // this histogram is about to get mutated,
                                                        // not sure this clone is necessary but it
                                                        // feels like it might be
    let _range_min = hist.keys().min().unwrap().clone(); // handle errors later
    let histogram_type = Distribution::from_str(histogram_type);

    let buckets = match histogram_type {
        Ok(Distribution::TimingDistribution) => generate_functional_buckets(2, 8, range_max),
        Ok(Distribution::MemoryDistribution) => generate_functional_buckets(2, 16, range_max),
        Ok(Distribution::CustomDistributionExponential) => {
            generate_exponential_buckets(0, 1_000, n_buckets)
        }
        Ok(Distribution::CustomDistributionLinear) => todo!(),
        _ => return Err("Invalid Histogram Type".to_string()),
    };

    fill_buckets(&mut hist, &buckets);
    transform_to_dirichlet_estimator(&mut hist, n_reporting);

    Ok(hist)
}

fn glam_style_histogram(filepath: &str) -> Vec<(String, HashMap<usize, f64>)> {
    let data = CsvReader::from_path(filepath).unwrap().finish().unwrap();

    let partitioned_data = data.partition_by(["build_id"]).unwrap();

    let mut results = Vec::new();

    for df in partitioned_data {
        let build_id = df
            .column("build_id")
            .unwrap()
            .str_value(0) // cow<str>
            .unwrap()
            .to_string();
        let client_level_dfs = df.partition_by(["client_id"]).unwrap();

        let mut client_levels = Vec::new();

        for d in client_level_dfs {
            let columns = d.select_series(&["key", "value"]).unwrap();
            let keys = columns[0].i64().unwrap().into_no_null_iter().collect();
            let values = columns[1].i64().unwrap().into_no_null_iter().collect();

            let client_aggregatted = map_sum_generic(keys, values);
            let client_normed = normalize_histogram_glam(client_aggregatted);

            client_levels.extend_from_slice(&client_normed);
        }

        let build_histograms = float_map_sum_tuples(client_levels);

        let dirichlet_transformed_hists = calculate_dirichlet_distribution(
            build_histograms,
            "custom_distribution_exponential",
            50,
        )
        .unwrap();

        results.push((build_id, dirichlet_transformed_hists));
    }
    results
}

#[pyfunction]
pub fn test_runner(csv_path: &str) -> PyResult<Vec<(String, HashMap<usize, f64>)>> {
    let results = glam_style_histogram(csv_path);

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_a_dirichlet() {
        let comp_hist: HashMap<usize, f64> = HashMap::from_iter([
            (0, (1.0 + (1.0 / 3.0)) / 3.0),
            (1, 1.0 / 9.0),
            (2, (2.0 + (1.0 / 3.0)) / 3.0),
        ]);

        let test_hist: HashMap<usize, f64> = HashMap::from_iter([(0, 1.0), (2, 2.0)]);

        let result = calculate_dirichlet_distribution(test_hist, "timing_distribution").unwrap();

        assert_eq!(comp_hist, result);
    }

    #[test]
    fn test_normalize_histogram() {
        let comp_hist: HashMap<usize, f64> = HashMap::from_iter([(2, 0.5), (11, 0.5)]);
        let input_hist: HashMap<i64, i64> = HashMap::from_iter([(11, 1), (2, 1)]);
        let test_hist_vec = normalize_histogram_glam(input_hist);
        let test_hist = HashMap::from_iter(test_hist_vec);

        assert_eq!(comp_hist, test_hist)
    }
    #[test]
    fn test_generate_functional_buckets() {
        let mut buckets = generate_functional_buckets(2, 8, 305);

        let target: Vec<usize> = vec![
            0usize, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 20, 22, 24, 26, 29,
            32, 34, 38, 41, 45, 49, 53, 58, 64, 69, 76, 82, 90, 98, 107, 117, 128, 139, 152, 165,
            181, 197, 215, 234, 256, 279, 304,
        ];

        buckets.sort();

        assert_eq!(target, buckets);
    }

    #[test]
    fn test_exponential_buckets_1() {
        let comp_buckets = vec![
            0usize, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 17, 20, 24, 29, 34, 40, 48, 57, 68, 81, 96,
            114, 135, 160, 190, 226, 268, 318, 378, 449, 533, 633, 752, 894, 1062, 1262, 1500,
            1782, 2117, 2516, 2990, 3553, 4222, 5017, 5961, 7083, 8416, 10000,
        ];

        let test_buckets = generate_exponential_buckets(0, 10000, 50);

        assert_eq!(comp_buckets, test_buckets);
    }

    #[test]
    fn test_exponential_buckets_2() {
        let comp_buckets = vec![0usize, 1, 3, 10, 32, 101, 319, 1006, 3172, 10000];
        let test_buckets = generate_exponential_buckets(0, 10000, 10);

        assert_eq!(comp_buckets, test_buckets);
    }

    #[test]
    fn test_exponential_buckets_3() {
        let comp_buckets = vec![0usize, 1, 3, 10, 32, 101, 319, 1006, 3172, 10000];
        let test_buckets = generate_exponential_buckets(1, 10000, 10);

        assert_eq!(test_buckets, comp_buckets);
    }
}
