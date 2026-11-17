//! Model weights stored as f16, computed in f32: half the bytes a matrix-vector product reads per
//! frame, which is where the mic models spend most of their memory traffic. tract picks a kernel
//! that takes f16 weights and f32 activations (`f16f32` packing; x86 needs AVX + FMA + F16C) from
//! the input types alone. Measurements: DEVLOG milestone 63 in the GamersEqualizer
//! repo (DeepFilterNet 3 10-18 % faster, the same output within 0.1 dB).
use tract_core::internal::*;
use tract_core::ops::einsum::EinSum;
use tract_core::ops::konst::Const;
use tract_core::ops::scan::Scan;

/// On wherever the CPU converts f16 in hardware; `GE_F16_WEIGHTS=0` keeps f32 (for comparing CPU
/// load), read when a model loads.
pub fn wanted() -> bool {
    std::env::var_os("GE_F16_WEIGHTS").is_none_or(|v| v != "0") && cpu_converts_f16()
}

/// Stores f32 constants that feed only matrix-vector products (`EinSum` in f32 whose other operand
/// is a single column) as f16, also inside `Scan` bodies, where DFN3 keeps its GRU recurrent
/// weights. Lowered convolutions (several columns) keep f32: their weights are small, so half the
/// bytes saves nothing, while the f16 kernels for them are slower. Run on a typed model before
/// `into_optimized`. Returns the bytes saved.
///
/// shortcut: the recurrent weight of `GruSeq` (packed by the op itself as f32) stays f32.
pub fn convert(model: &mut TypedModel) -> TractResult<usize> {
    let mut saved = 0;
    for id in 0..model.nodes.len() {
        if let Some(scan) = model.node(id).op_as::<Scan>() {
            let mut scan = scan.clone();
            let inner = convert(&mut scan.body)?;
            if inner > 0 {
                saved += inner;
                model.node_mut(id).op = Box::new(scan);
            }
            continue;
        }
        let node = model.node(id);
        let Some(konst) = node.op_as::<Const>() else { continue };
        let val = konst.val().clone();
        let succ = &node.outputs[0].successors;
        let only_matmuls = !succ.is_empty()
            && succ.iter().all(|s| {
                model.node(s.node).op_as::<EinSum>().is_some_and(|e| {
                    e.operating_dt == f32::datum_type()
                        && e.q_params.is_none()
                        && columns(model, s.node, e, s.slot) == Some(1)
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

/// Columns of the product: output elements along axes the weight (input `slot`) lacks. `None`
/// when a dimension is symbolic.
fn columns(model: &TypedModel, node: usize, einsum: &EinSum, slot: usize) -> Option<usize> {
    let shape = model.outlet_fact(OutletId::new(node, 0)).ok()?.shape.as_concrete()?.to_vec();
    let mut n = 1;
    for axis in einsum.axes.iter_all_axes() {
        if axis.inputs.get(slot).is_some_and(|p| p.is_empty()) {
            for &pos in axis.outputs.first()? {
                n *= shape.get(pos)?;
            }
        }
    }
    Some(n)
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
