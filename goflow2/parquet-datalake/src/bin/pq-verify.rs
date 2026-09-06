// Quick sanity-check reader: prints schema, row count, and a few sample
// rows from a Parquet file. Only exists because this host has neither
// duckdb nor pyarrow available for a one-off manual check.
use std::error::Error;
use std::fs::File;

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

fn main() -> Result<(), Box<dyn Error>> {
    let path = std::env::args().nth(1).expect("usage: pq-verify <file.parquet>");
    let file = File::open(&path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    println!("Schema: {:#?}", builder.schema());
    let mut reader = builder.build()?;

    let mut total_rows = 0usize;
    let mut shown = false;
    while let Some(batch) = reader.next().transpose()? {
        total_rows += batch.num_rows();
        if !shown {
            let head = batch.slice(0, batch.num_rows().min(5));
            println!("{}", arrow::util::pretty::pretty_format_batches(&[head])?);
            shown = true;
        }
    }
    println!("Total rows: {total_rows}");
    Ok(())
}
