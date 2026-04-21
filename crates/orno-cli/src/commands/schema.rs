use anyhow::Result;

pub fn run() -> Result<()> {
    let s = orno_core::pipeline_json_schema_string()?;
    println!("{s}");
    Ok(())
}
