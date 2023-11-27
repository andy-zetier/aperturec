use anyhow::Result;
use plotters::prelude::*;
use std::path::Path;

pub(crate) fn plot_csv(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    caption: impl AsRef<str>,
) -> Result<()> {
    let reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_path(input)?;
    let points = reader
        .into_deserialize()
        .collect::<Result<Vec<(usize, usize)>, _>>()?;

    let min_x = points.iter().min_by_key(|(x, _)| x).unwrap().0;
    let min_y = points.iter().min_by_key(|(_, y)| y).unwrap().1;
    let max_x = points.iter().max_by_key(|(x, _)| x).unwrap().0;
    let max_y = points.iter().max_by_key(|(_, y)| y).unwrap().1;

    let root = BitMapBackend::new(&output, (800, 600)).into_drawing_area();
    root.fill(&WHITE)?;
    let root = root.margin(10, 10, 40, 10);
    let mut chart = ChartBuilder::on(&root)
        .caption(caption, ("sans-serif", 40).into_font())
        .x_label_area_size(20)
        .y_label_area_size(20)
        .build_cartesian_2d(min_x..max_x, min_y..max_y)?;
    chart.configure_mesh().x_labels(10).y_labels(10).draw()?;

    chart.draw_series(LineSeries::new(points, &RED))?;

    root.present()?;
    Ok(())
}
