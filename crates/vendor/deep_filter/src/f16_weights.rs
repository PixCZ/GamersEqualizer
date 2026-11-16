//! Model weights stored as f16, computed in f32: half the bytes a matrix-vector product reads per
//! frame. Experiment, see `docs/mic-cpu-analysis.md` chapter 8 in the GamersEqualizer repo.
//! tract picks a kernel that takes f16 weights and f32 activations (`f16f32` packing of
//! `fma_mmm_f32_32x1` on x86, AVX + FMA + F16C) from the input types alone.
use tract_core::internal::*;
use tract_core::ops::einsum::EinSum;
use tract_core::ops::konst::Const;

/// Temporary switch while the experiment runs: `GE_F16_WEIGHTS=1`, read when a model loads.
pub fn wanted() -> bool {
    std::env::var_os("GE_F16_WEIGHTS").is_some_and(|v| v == "1") && cpu_converts_f16()
}

/// Stores every f32 constant that feeds only matrix products (`EinSum` in f32) as f16.
/// Run on a typed model before `into_optimized`. Returns the bytes saved.
///
/// shortcut: constants inside `Scan` bodies, convolution kernels and the recurrent weight of
/// `GruSeq` (packed by the op itself as f32) stay f32; extend when the measurement pays off.
pub fn convert(model: &mut TypedModel) -> TractResult<usize> {
    let mut saved = 0;
    for id in 0..model.nodes.len() {
        let node = model.node(id);
        let Some(konst) = node.op_as::<Const>() else { continue };
        let val = konst.val().clone();
        let succ = &node.outputs[0].successors;
        let only_matmuls = !succ.is_empty()
            && succ.iter().all(|s| {
                model.node(s.node).op_as::<EinSum>().is_some_and(|e| {
                    e.operating_dt == f32::datum_type() && e.q_params.is_none()
                })
            });
        if val.datum_type() != f32::datum_type()
            || val.rank() < 2
            || !val.is_plain_ram()
            || !only_matmuls
        {
            continue;
        }
        let half = val.cast_to::<f16>()?.into_owned().into_arc_tensor();
        model.node_mut(id).op = Box::new(Const::new(half)?);
        saved += val.len() * 2;
    }
    model.refresh_output_facts()?;
    Ok(saved)
}

/// f32 constant bytes in the model by the op that reads them: what `convert` reaches and what not.
pub fn weight_bytes_by_consumer(model: &TypedModel) -> Vec<(String, usize)> {
    let mut by_op = std::collections::BTreeMap::<String, usize>::new();
    for node in &model.nodes {
        let Some(konst) = node.op_as::<Const>() else { continue };
        let val = konst.val();
        if !val.datum_type().is_float() || val.len() < 64 {
            continue;
        }
        let consumer = node.outputs[0]
            .successors
            .first()
            .map(|s| model.node(s.node).op().name().to_string())
            .unwrap_or_default();
        let key = format!("{:?} -> {consumer}", val.datum_type());
        *by_op.entry(key).or_default() += val.len() * val.datum_type().size_of();
    }
    by_op.into_iter().collect()
}

fn cpu_converts_f16() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("f16c") && is_x86_feature_detected!("fma")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        true
    }
}
