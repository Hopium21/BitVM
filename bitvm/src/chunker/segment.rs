use super::assigner::BCAssigner;
use super::common;
use super::common::*;
use super::elements::ElementTrait;
use crate::bn254::utils::Hint;
use crate::execute_script;
use crate::treepp::*;
use std::rc::Rc;

/// Each segment is a branch in the taproot of disprove transaction.
#[derive(Debug)]
pub struct Segment {
    pub name: String,
    pub script: Script,
    pub parameter_list: Vec<Rc<Box<dyn ElementTrait>>>,
    pub result_list: Vec<Rc<Box<dyn ElementTrait>>>,
    pub hints: Vec<Hint>,
    pub final_segment: bool,
}

/// After the returned `script` and `witness` are executed together, only `OP_FALSE` left on the stack.
/// If operator gives a wrong intermediate value, `OP_TRUE` will left on the stack and challenger will finish the slash.
impl Segment {
    fn hinted_to_witness(&self) -> Vec<Vec<u8>> {
        let res = execute_script(script! {
            for hint in self.hints.iter() {
                { hint.push() }
            }
        });
        res.final_stack.0.iter_str().fold(vec![], |mut vector, x| {
            vector.push(x);
            vector
        })
    }

    pub fn new(script: Script) -> Self {
        Self::new_with_name(String::new(), script)
    }

    pub fn new_with_name(name: String, script: Script) -> Self {
        Self {
            name,
            script,
            parameter_list: vec![],
            result_list: vec![],
            hints: vec![],
            final_segment: false,
        }
    }

    pub fn add_parameter<T: ElementTrait + 'static + Clone>(mut self, x: &T) -> Self {
        self.parameter_list.push(Rc::new(Box::new(x.clone())));
        self
    }

    pub fn add_result<T: ElementTrait + 'static + Clone>(mut self, x: &T) -> Self {
        self.result_list.push(Rc::new(Box::new(x.clone())));
        self
    }

    pub fn add_hint(mut self, hints: Vec<Hint>) -> Self {
        self.hints = hints;
        self
    }

    pub fn mark_final(mut self) -> Self {
        self.final_segment = true;
        self
    }

    pub fn is_final(&self) -> bool {
        self.final_segment
    }

    /// Create script, and the coressponding witness hopes to be like below.
    /// [hinted, input0, input1, input1_bc_witness, input0_bc_witness, outpu0_bc_witness, output1_bc_witness]
    pub fn script<T: BCAssigner>(&self, assigner: &T) -> Script {
        let mut base: usize = 0;
        let mut script = script! {

            // 1. unlock all bitcommitment
            for result in self.result_list.iter().rev() {
                {assigner.locking_script(result)}
                for _ in 0..BLAKE3_HASH_LENGTH {
                    OP_TOALTSTACK
                }
            }
            for parameter in self.parameter_list.iter() {
                {assigner.locking_script(parameter)} // verify bit commitment
                // move all original data when verifying the proof
                if common::PROOF_NAMES.contains(&parameter.id()) {
                    for _ in 0..parameter.as_ref().witness_size() {
                        OP_TOALTSTACK
                    }
                }
                else {
                    for _ in 0..BLAKE3_HASH_LENGTH {
                        OP_TOALTSTACK
                    }
                }
            }
        };

        for parameter in self.parameter_list.iter().rev() {
            let parameter_length = parameter.as_ref().witness_size();

            // skip hash when verifying the proof
            if common::PROOF_NAMES.contains(&parameter.id()) {
                script = script.push_script(
                    script! {
                        for _ in 0..parameter_length {
                            {base + parameter_length - 1} OP_PICK
                        }
                        for _ in 0..parameter_length {
                            OP_FROMALTSTACK
                        }
                        {equalverify(parameter_length)}
                    }
                    .compile(),
                );
            } else {
                script = script.push_script(
                    script! {
                    // 2. push parameters onto altstack
                        for _ in 0..parameter_length {
                            {base + parameter_length - 1} OP_PICK
                        }
                        {blake3_var_length(parameter_length)}
                        for _ in 0..BLAKE3_HASH_LENGTH {
                            OP_FROMALTSTACK
                        }
                        {equalverify(BLAKE3_HASH_LENGTH)}
                    }
                    .compile(),
                );
            }

            base += parameter_length;
        }

        script = script.push_script(
            script! {

                // 3. run inner script
                {self.script.clone()}

                // 4. result of blake3
                for result in self.result_list.iter().rev() {
                    {blake3_var_length(result.as_ref().witness_size())}
                    for _ in 0..BLAKE3_HASH_LENGTH {
                        OP_TOALTSTACK
                    }
                }

                for _ in 0..BLAKE3_HASH_LENGTH * self.result_list.len() * 2 {
                    OP_FROMALTSTACK
                }
            }
            .compile(),
        );

        if !self.final_segment {
            script = script.push_script(
                script! {
                // 5. compare the result with assigned value
                {common::not_equal(BLAKE3_HASH_LENGTH * self.result_list.len())}
                }
                .compile(),
            );
        }
        script
    }

    /// Create witness.
    pub fn witness<T: BCAssigner>(&self, assigner: &T) -> RawWitness {
        // [hinted, input0, input1, input1_bc_witness, input0_bc_witness, output1_bc_witness, outpu0_bc_witness]
        let mut witness = vec![];

        witness.append(&mut self.hinted_to_witness());

        for parameter in self.parameter_list.iter() {
            match parameter.as_ref().to_witness() {
                Some(mut w) => {
                    witness.append(&mut w);
                }
                None => {
                    panic!("extract witness {} fail in {}", parameter.id(), self.name)
                }
            }
        }

        for parameter in self.parameter_list.iter().rev() {
            witness.append(&mut assigner.get_witness(parameter));
        }

        for result in self.result_list.iter() {
            witness.append(&mut assigner.get_witness(result))
        }

        witness
    }
}

#[cfg(test)]
mod tests {
    use ark_ff::UniformRand;
    use ark_std::test_rng;
    use rand::{RngCore as _, SeedableRng as _};

    use super::Segment;
    use crate::chunker::elements::DataType::G1PointData;
    use crate::chunker::elements::DataType::G2PointData;
    use crate::chunker::elements::{ElementTrait, G1PointType, G2PointType};
    use crate::chunker::{assigner::DummyAssigner, elements::DataType::Fq6Data, elements::Fq6Type};
    use crate::{execute_script_with_inputs, treepp::*};

    #[test]
    fn test_segment_by_simple_case() {
        let mut assigner = DummyAssigner::default();

        let mut a0 = Fq6Type::new(&mut assigner, "a0");
        a0.fill_with_data(Fq6Data(ark_bn254::Fq6::from(1)));

        let segment = Segment::new(script! {}).add_parameter(&a0).add_result(&a0);

        let script = segment.script(&assigner);
        let witness = segment.witness(&assigner);

        println!("witnesss needs stack {}", witness.len());
        println!(
            "element witnesss needs stack {}",
            a0.to_hash_witness().unwrap().len()
        );

        let res = execute_script_with_inputs(script, witness);
        println!("res.success {}", res.success);
        println!("res.stack len {}", res.final_stack.len());
        println!("rse.remaining: {}", res.remaining_script);
        println!("res: {:1000}", res);
    }

    #[test]
    fn test_segment_by_proof_case() {
        let mut rng = ark_std::rand::rngs::StdRng::seed_from_u64(test_rng().next_u64());

        let mut assigner = DummyAssigner::default();

        let mut a0 = Fq6Type::new(&mut assigner, "scalar_1");
        a0.fill_with_data(Fq6Data(ark_bn254::Fq6::from(1)));

        let mut a1 = G1PointType::new(&mut assigner, "a0");
        a1.fill_with_data(G1PointData(ark_bn254::G1Affine::rand(&mut rng)));

        let segment = Segment::new(script! {
            for _ in 0..54 {
                OP_DROP
            }
        })
        .add_parameter(&a1)
        .add_parameter(&a0)
        .add_result(&a1);

        let script = segment.script(&assigner);
        let witness = segment.witness(&assigner);

        println!("witnesss needs stack {}", witness.len());
        println!(
            "element witnesss needs stack {}",
            a0.to_hash_witness().unwrap().len()
        );

        let res = execute_script_with_inputs(script, witness);
        println!("res.success {}", res.success);
        println!("res.stack len {}", res.final_stack.len());
        println!("rse.remaining: {}", res.remaining_script);
        println!("res: {:1000}", res);
    }

    #[test]
    fn test_sgement_by_point_case() {
        let mut rng = ark_std::rand::rngs::StdRng::seed_from_u64(test_rng().next_u64());
        let mut assigner = DummyAssigner::default();

        let x = ark_bn254::G2Affine::rand(&mut rng);

        let mut q4 = G2PointType::new(&mut assigner, "q4");
        q4.fill_with_data(G2PointData(x));

        let mut t4 = G2PointType::new(&mut assigner, "t4_init");
        t4.fill_with_data(G2PointData(x));

        let segment = Segment::new_with_name("copy_q4_to_t4".into(), script! {})
            .add_parameter(&q4)
            .add_result(&t4);

        let script = segment.script(&assigner);
        let witness = segment.witness(&assigner);

        println!("witnesss needs stack {}", witness.len());

        let res = execute_script_with_inputs(script, witness);
        println!("res.success {}", res.success);
        println!("res.stack len {}", res.final_stack.len());
        println!("rse.remaining: {}", res.remaining_script);
        println!("res: {:1000}", res);
    }
}
