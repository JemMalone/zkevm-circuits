use crate::arith_helpers::*;
use crate::common::{LANE_SIZE, ROTATION_CONSTANTS};
use crate::gates::{
    gate_helpers::*,
    rho_helpers::*,
    tables::{Base13toBase9TableConfig, SpecialChunkTableConfig},
};
use halo2::{
    circuit::{Layouter, Region},
    plonk::{
        Advice, Column, ConstraintSystem, Error, Expression, Fixed, Selector,
    },
    poly::Rotation,
};
use num_bigint::BigUint;
use num_traits::{One, Zero};
use pasta_curves::arithmetic::FieldExt;
use std::iter;
use std::marker::PhantomData;

#[derive(Debug, Clone)]
struct RunningSumAdvices {
    coef: Column<Advice>,
    acc: Column<Advice>,
}
#[derive(Debug, Clone)]
pub struct BlockCountAdvices {
    block_count: Column<Advice>,
    step2_acc: Column<Advice>,
    step3_acc: Column<Advice>,
}

#[derive(Debug, Clone)]
pub struct RhoAdvices {
    input: RunningSumAdvices,
    output: RunningSumAdvices,
    bc: BlockCountAdvices,
}

impl From<[Column<Advice>; 7]> for RhoAdvices {
    fn from(cols: [Column<Advice>; 7]) -> Self {
        let input = RunningSumAdvices {
            coef: cols[0],
            acc: cols[1],
        };
        let output = RunningSumAdvices {
            coef: cols[2],
            acc: cols[3],
        };
        let bc = BlockCountAdvices {
            block_count: cols[4],
            step2_acc: cols[5],
            step3_acc: cols[6],
        };
        Self { input, output, bc }
    }
}

#[derive(Debug, Clone)]
struct RotatingVariables {
    rotation: u32,
    chunk_idx: u32,
    step: u32,
    input_raw: BigUint,
    input_coef: BigUint,
    input_power_of_base: BigUint,
    input_acc: BigUint,
    output_coef: BigUint,
    output_power_of_base: BigUint,
    output_acc: BigUint,
    block_count: Option<u32>,
    // step2 acc and step3 acc
    block_count_acc: [u32; 2],
    low_value: u64,
    high_value: Option<u64>,
}

impl RotatingVariables {
    fn from(lane_base_13: BigUint, rotation: u32) -> Result<Self, Error> {
        let chunk_idx = 1;
        let step = get_step_size(chunk_idx, rotation);
        let input_raw = lane_base_13.clone() / B13;
        let input_coef = input_raw.clone() % B13.pow(step);
        let input_power_of_base = BigUint::from(B13);
        let input_acc = lane_base_13.clone();
        let (block_count, output_coef) =
            get_block_count_and_output_coef(input_coef.clone());
        let output_coef = BigUint::from(output_coef);
        let output_power_of_base = if is_at_rotation_offset(chunk_idx, rotation)
        {
            BigUint::one()
        } else {
            BigUint::from(B9).pow(rotation + chunk_idx)
        };
        let output_acc = BigUint::zero();
        let mut block_count_acc = [0; 2];
        if step == 2 || step == 3 {
            block_count_acc[(step - 2) as usize] += block_count;
        }
        let low_value: u64 = biguint_mod(&lane_base_13, B13);
        Ok(Self {
            rotation,
            chunk_idx,
            step,
            input_raw,
            input_coef,
            input_power_of_base,
            input_acc,
            output_coef,
            output_power_of_base,
            output_acc,
            block_count: Some(block_count),
            block_count_acc,
            low_value,
            high_value: None,
        })
    }

    fn next(&self) -> Self {
        assert!(self.chunk_idx < LANE_SIZE);
        let mut new = self.clone();
        new.chunk_idx += self.step;
        new.step = get_step_size(new.chunk_idx, self.rotation);
        new.input_raw /= B13.pow(self.step);
        new.input_coef = new.input_raw.clone() % B13.pow(new.step);
        new.input_power_of_base *= B13.pow(self.step);
        new.input_acc -=
            self.input_power_of_base.clone() * self.input_coef.clone();
        new.output_power_of_base =
            if is_at_rotation_offset(new.chunk_idx, self.rotation) {
                BigUint::one()
            } else {
                self.output_power_of_base.clone() * B9.pow(self.step)
            };
        new.output_acc +=
            self.output_power_of_base.clone() * self.output_coef.clone();
        // Case of last chunk, aka special chunks
        if new.chunk_idx >= LANE_SIZE {
            assert!(
                new.input_raw.is_zero(),
                "Expect raw input at last chunk should be zero, but got {:?}",
                new.input_raw
            );
            new.block_count = None;
            new.high_value = Some(biguint_mod(&self.input_raw, B13));
            let high = new.high_value.unwrap();
            new.output_coef =
                BigUint::from(convert_b13_coef(high + self.low_value));
            let expect =
                new.low_value + high * BigUint::from(B13).pow(LANE_SIZE);
            assert_eq!(
                new.input_acc,
                expect,
                "input_acc got: {:?}  expect: {:?} = low({:?}) + high({:?}) * 13**64",
                new.input_acc,
                expect,
                new.low_value,
                high,
            );
            return new;
        }
        let (block_count, usual_output_coef) =
            get_block_count_and_output_coef(new.input_coef.clone());
        new.output_coef = BigUint::from(usual_output_coef);
        new.block_count = Some(block_count);
        if self.step == 2 || self.step == 3 {
            new.block_count_acc[(self.step - 2) as usize] += block_count;
        }
        new
    }
    fn finalize(&self) -> Self {
        let mut new = self.clone();
        new.output_acc +=
            self.output_power_of_base.clone() * self.output_coef.clone();
        new
    }
}

#[derive(Debug, Clone)]
pub struct LaneRotateConversionConfig<F> {
    q_enable: Selector,
    q_is_special: Selector,
    adv: RhoAdvices,
    chunk_rotate_convert_configs: Vec<ChunkRotateConversionConfig<F>>,
    special_chunk_config: SpecialChunkConfig<F>,
    lane_xy: (usize, usize),
    rotation: u32,
}

impl<F: FieldExt> LaneRotateConversionConfig<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        lane_xy: (usize, usize),
        adv: RhoAdvices,
        axiliary: [Column<Advice>; 2],
        base13_to_9: [Column<Fixed>; 3],
        special: [Column<Fixed>; 2],
    ) -> Self {
        meta.enable_equality(adv.input.acc.into());
        meta.enable_equality(adv.output.acc.into());
        let q_enable = meta.selector();
        let q_is_special = meta.selector();
        let rotation = ROTATION_CONSTANTS[lane_xy.0][lane_xy.1];
        let slices = slice_lane(rotation);
        let chunk_rotate_convert_configs = slices
            .iter()
            .map(|(chunk_idx, step)| {
                ChunkRotateConversionConfig::configure(
                    q_enable,
                    meta,
                    adv.clone(),
                    base13_to_9,
                    *chunk_idx,
                    rotation,
                    *step,
                )
            })
            .collect::<Vec<_>>();
        let special_chunk_config = SpecialChunkConfig::configure(
            meta,
            q_is_special,
            adv.input.acc,
            adv.output.acc,
            axiliary[0],
            special,
            rotation as u64,
        );

        Self {
            q_enable,
            q_is_special,
            adv,
            chunk_rotate_convert_configs,
            special_chunk_config,
            lane_xy,
            rotation,
        }
    }
    pub fn assign_region(
        &self,
        layouter: &mut impl Layouter<F>,
        lane_base_13: &Lane<F>,
    ) -> Result<(Lane<F>, BlockCount2<F>), Error> {
        let (lane, block_counts) = layouter.assign_region(
            || format!("LRCC {:?}", self.lane_xy),
            |mut region| {
                let mut offset = 0;
                let cell = region.assign_advice(
                    || "base_13_col",
                    self.adv.input.acc,
                    offset,
                    || Ok(lane_base_13.value),
                )?;
                region.constrain_equal(lane_base_13.cell, cell)?;

                offset += 1;

                let mut rv = RotatingVariables::from(
                    f_to_biguint(lane_base_13.value)
                        .ok_or(Error::SynthesisError)?,
                    self.rotation,
                )?;
                let all_block_counts: Result<Vec<BlockCount2<F>>, Error> = self
                    .chunk_rotate_convert_configs
                    .iter()
                    .map(|config| {
                        let block_counts = config.assign_region(
                            &mut region,
                            offset,
                            &mut rv,
                        )?;
                        offset += 1;
                        rv = rv.next();
                        Ok(block_counts)
                    })
                    .collect();
                let all_block_counts = all_block_counts?;
                let block_counts =
                    all_block_counts.last().ok_or(Error::SynthesisError)?;
                let lane = self.special_chunk_config.assign_region(
                    &mut region,
                    offset,
                    &rv,
                )?;
                Ok((lane, *block_counts))
            },
        )?;
        Ok((lane, block_counts))
    }

    pub fn load(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        self.chunk_rotate_convert_configs[0]
            .base_13_to_base_9_lookup
            .load(layouter)?;
        self.special_chunk_config
            .special_chunk_table_config
            .load(layouter)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ChunkRotateConversionConfig<F> {
    q_enable: Selector,
    adv: RhoAdvices,
    base_13_to_base_9_lookup: Base13toBase9TableConfig<F>,
    block_count_acc_config: BlockCountAccConfig<F>,
    chunk_idx: u32,
    rotation: u32,
    step: u32,
}

impl<F: FieldExt> ChunkRotateConversionConfig<F> {
    fn configure(
        q_enable: Selector,
        meta: &mut ConstraintSystem<F>,
        adv: RhoAdvices,
        fix_cols: [Column<Fixed>; 3],
        chunk_idx: u32,
        rotation: u32,
        step: u32,
    ) -> Self {
        let base_13_to_base_9_lookup = Base13toBase9TableConfig::configure(
            meta,
            q_enable,
            adv.input.coef,
            adv.output.coef,
            adv.bc.block_count,
            fix_cols,
        );

        meta.create_gate("Running down input", |meta| {
            let q_enable = meta.query_selector(q_enable);
            let coef = meta.query_advice(adv.input.coef, Rotation::cur());
            let power_of_base = Expression::Constant(F::from(B13).pow(&[
                chunk_idx.into(),
                0,
                0,
                0,
            ]));
            let delta_acc = meta.query_advice(adv.input.acc, Rotation::next())
                - meta.query_advice(adv.input.acc, Rotation::cur());

            vec![(
                "delta_acc === - coef * power_of_base", // running down for input
                q_enable * (delta_acc + coef * power_of_base),
            )]
        });
        meta.create_gate("Running up for output", |meta| {
            let q_enable = meta.query_selector(q_enable);
            let coef = meta.query_advice(adv.output.coef, Rotation::cur());
            let power_of_base = F::from(B9).pow(&[
                ((rotation + chunk_idx) % LANE_SIZE).into(),
                0,
                0,
                0,
            ]);
            let power_of_base = Expression::Constant(power_of_base);
            let delta_acc = meta.query_advice(adv.output.acc, Rotation::next())
                - meta.query_advice(adv.output.acc, Rotation::cur());
            vec![(
                "delta_acc === coef * power_of_base", // running up for output
                q_enable * (delta_acc - coef * power_of_base),
            )]
        });

        let block_count_acc_config = BlockCountAccConfig::configure(
            meta,
            q_enable,
            adv.bc.clone(),
            step,
        );

        Self {
            q_enable,
            adv,
            base_13_to_base_9_lookup,
            block_count_acc_config,
            chunk_idx,
            rotation,
            step,
        }
    }

    fn assign_region(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        rv: &mut RotatingVariables,
    ) -> Result<BlockCount2<F>, Error> {
        region.assign_advice(
            || "Input Coef",
            self.adv.input.coef,
            offset,
            || {
                biguint_to_f::<F>(rv.input_coef.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;
        region.assign_advice(
            || "Input accumulator",
            self.adv.input.acc,
            offset,
            || {
                biguint_to_f::<F>(rv.input_acc.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;
        region.assign_advice(
            || "Output Coef",
            self.adv.output.coef,
            offset,
            || {
                biguint_to_f::<F>(rv.output_coef.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;
        region.assign_advice(
            || "Output accumulator",
            self.adv.output.acc,
            offset,
            || {
                biguint_to_f::<F>(rv.output_acc.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;
        let block_counts = self.block_count_acc_config.assign_region(
            region,
            offset,
            rv.block_count.ok_or(Error::SynthesisError)?,
            rv.block_count_acc,
        )?;
        Ok(block_counts)
    }
}

#[derive(Debug, Clone)]
pub struct SpecialChunkConfig<F> {
    q_enable: Selector,
    last_b9_coef: Column<Advice>,
    rotation: u64,
    base_13_acc: Column<Advice>,
    base_9_acc: Column<Advice>,
    special_chunk_table_config: SpecialChunkTableConfig<F>,
}

impl<F: FieldExt> SpecialChunkConfig<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        q_enable: Selector,
        base_13_acc: Column<Advice>,
        base_9_acc: Column<Advice>,
        last_b9_coef: Column<Advice>,
        special: [Column<Fixed>; 2],
        rotation: u64,
    ) -> Self {
        meta.create_gate("validate base_9_acc", |meta| {
            let delta_base_9_acc = meta
                .query_advice(base_9_acc, Rotation::next())
                - meta.query_advice(base_9_acc, Rotation::cur());
            let last_b9_coef = meta.query_advice(last_b9_coef, Rotation::cur());
            let pow_of_9 =
                Expression::Constant(F::from_u64(B9).pow(&[rotation, 0, 0, 0]));
            vec![(
                "delta_base_9_acc === (high_value + low_value) * 9**rotation",
                meta.query_selector(q_enable)
                    * (delta_base_9_acc - last_b9_coef * pow_of_9),
            )]
        });
        let special_chunk_table_config = SpecialChunkTableConfig::configure(
            meta,
            q_enable,
            base_13_acc,
            last_b9_coef,
            special,
        );
        Self {
            q_enable,
            last_b9_coef,
            rotation,
            base_13_acc,
            base_9_acc,
            special_chunk_table_config,
        }
    }
    fn assign_region(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        rv: &RotatingVariables,
    ) -> Result<Lane<F>, Error> {
        self.q_enable.enable(region, offset)?;
        rv.high_value.ok_or(Error::SynthesisError).unwrap();
        region.assign_advice(
            || "input_acc",
            self.base_13_acc,
            offset,
            || {
                biguint_to_f::<F>(rv.input_acc.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;
        region.assign_advice(
            || "ouput_acc",
            self.base_9_acc,
            offset,
            || {
                biguint_to_f::<F>(rv.output_acc.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;
        region.assign_advice(
            || "last_b9_coef",
            self.last_b9_coef,
            offset,
            || {
                biguint_to_f::<F>(rv.output_coef.clone())
                    .ok_or(Error::SynthesisError)
            },
        )?;

        let rv_final = rv.finalize();
        region.assign_advice(
            || "input_acc",
            self.base_13_acc,
            offset + 1,
            || Ok(F::zero()),
        )?;
        let value = biguint_to_f::<F>(rv_final.output_acc)
            .ok_or(Error::SynthesisError)?;
        let cell = region.assign_advice(
            || "input_acc",
            self.base_9_acc,
            offset + 1,
            || Ok(value),
        )?;

        Ok(Lane { cell, value })
    }
}

#[derive(Debug, Clone)]
pub struct BlockCountAccConfig<F> {
    q_enable: Selector,
    bc: BlockCountAdvices,
    step: u32,
    _marker: PhantomData<F>,
}

impl<F: FieldExt> BlockCountAccConfig<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        q_enable: Selector,
        bc: BlockCountAdvices,
        step: u32,
    ) -> Self {
        meta.create_gate("accumulate block count", |meta| {
            let q_enable = meta.query_selector(q_enable);
            let block_count =
                meta.query_advice(bc.block_count, Rotation::cur());
            let delta_step2 = meta.query_advice(bc.step2_acc, Rotation::next())
                - meta.query_advice(bc.step2_acc, Rotation::cur());
            let delta_step3 = meta.query_advice(bc.step3_acc, Rotation::next())
                - meta.query_advice(bc.step3_acc, Rotation::cur());

            match step {
                1 | 4 => vec![
                    ("block_count = 0", block_count),
                    ("delta_step2 = 0", delta_step2),
                    ("delta_step3 = 0", delta_step3),
                ],
                2 => vec![
                    ("delta_step2 = block_count", delta_step2 - block_count),
                    ("delta_step3 = 0", delta_step3),
                ],
                3 => vec![
                    ("delta_step2 = 0", delta_step2),
                    ("delta_step3 = block_count", delta_step3 - block_count),
                ],
                _ => {
                    unreachable!("expect step < 4");
                }
            }
            .iter()
            .map(|(name, poly)| (*name, q_enable.clone() * poly.clone()))
            .collect::<Vec<_>>()
        });

        Self {
            q_enable,
            bc,
            step,
            _marker: PhantomData,
        }
    }

    pub fn assign_region(
        &self,
        region: &mut Region<'_, F>,
        offset: usize,
        block_count: u32,
        block_count_acc: [u32; 2],
    ) -> Result<BlockCount2<F>, Error> {
        let block_count = F::from_u64(block_count.into());
        let acc = block_count_acc.map(|x| F::from_u64(x.into()));
        region.assign_advice(
            || "block_count",
            self.bc.block_count,
            offset,
            || Ok(block_count),
        )?;
        let cell = region.assign_advice(
            || "step 2 bc acc",
            self.bc.step2_acc,
            offset,
            || Ok(acc[0]),
        )?;
        let step2 = BlockCount {
            cell,
            value: acc[0],
        };
        let cell = region.assign_advice(
            || "step 3 bc acc",
            self.bc.step3_acc,
            offset,
            || Ok(acc[1]),
        )?;
        let step3 = BlockCount {
            cell,
            value: acc[1],
        };
        Ok((step2, step3))
    }
}

#[derive(Clone)]
pub struct BlockCountFinalConfig<F> {
    q_enable: Selector,
    block_count_cols: [Column<Advice>; 2],
    _marker: PhantomData<F>,
}
impl<F: FieldExt> BlockCountFinalConfig<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        block_count_cols: [Column<Advice>; 2],
    ) -> Self {
        let q_enable = meta.selector();
        for column in block_count_cols.iter() {
            meta.enable_equality((*column).into());
        }

        meta.create_gate("block count final check", |meta| {
            let q_enable = meta.query_selector(q_enable);
            let step2_acc =
                meta.query_advice(block_count_cols[0], Rotation::cur());
            let step3_acc =
                meta.query_advice(block_count_cols[1], Rotation::cur());
            iter::empty()
                .chain(Some((
                    "step2_acc <=12",
                    (0..=12)
                        .map(|x| {
                            step2_acc.clone() - Expression::Constant(F::from(x))
                        })
                        .reduce(|a, b| a * b),
                )))
                .chain(Some((
                    "step3_acc <= 13 * 13",
                    (0..=13 * 13)
                        .map(|x| {
                            step3_acc.clone() - Expression::Constant(F::from(x))
                        })
                        .reduce(|a, b| a * b),
                )))
                .map(|(name, poly)| match poly {
                    Some(poly) => (name, q_enable.clone() * poly),
                    None => (name, Expression::Constant(F::zero())),
                })
                .collect::<Vec<_>>()
        });

        Self {
            q_enable,
            block_count_cols,
            _marker: PhantomData,
        }
    }
    pub fn assign_region(
        &self,
        layouter: &mut impl Layouter<F>,
        block_count_cells: [BlockCount2<F>; 25],
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "final block count",
            |mut region| {
                for (offset, bc) in block_count_cells.iter().enumerate() {
                    self.q_enable.enable(&mut region, offset)?;
                    let cell_1 = region.assign_advice(
                        || format!("block_count step2 acc lane {}", offset),
                        self.block_count_cols[0],
                        offset,
                        || Ok(bc.0.value),
                    )?;
                    region.constrain_equal(cell_1, bc.0.cell)?;
                    let cell_2 = region.assign_advice(
                        || format!("block_count step3 acc lane {}", offset),
                        self.block_count_cols[1],
                        offset,
                        || Ok(bc.1.value),
                    )?;
                    region.constrain_equal(cell_2, bc.1.cell)?;
                }
                Ok(())
            },
        )?;
        Ok(())
    }
}
