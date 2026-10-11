// Generated from ONNX "model.onnx" by burn-onnx
extern crate alloc;
use burn::prelude::*;
use burn::nn::LayerNorm;
use burn::nn::LayerNormConfig;
use burn::nn::Linear;
use burn::nn::LinearConfig;
use burn::tensor::Bytes;
use burn::tensor::TensorData;
use burn_store::BurnpackStore;
use burn_store::ModuleSnapshot;


#[derive(Module, Debug)]
pub struct Submodule1 {
    constant148: burn::module::Param<Tensor<1, Int>>,
    constant149: burn::module::Param<Tensor<1>>,
    constant1: burn::module::Param<Tensor<2>>,
    layernormalization1: LayerNorm,
    linear1: Linear,
    constant172: burn::module::Param<Tensor<1, Int>>,
    constant166: burn::module::Param<Tensor<3>>,
    constant204: burn::module::Param<Tensor<1>>,
    linear2: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule1 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let constant148: burn::module::Param<Tensor<1, Int>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
                Int,
            >::from_data(
                burn::tensor::TensorData::from([-1i64]),
                (device, burn::tensor::DType::I64),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant149: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1: burn::module::Param<Tensor<2>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                2,
            >::zeros([262152, 768], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [262152, 768].into(),
        );
        let layernormalization1 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear1 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant172: burn::module::Param<Tensor<1, Int>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
                Int,
            >::from_data(
                burn::tensor::TensorData::from([-1i64]),
                (device, burn::tensor::DType::I64),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant166: burn::module::Param<Tensor<3>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                3,
            >::zeros([1, 32, 1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1, 32, 1].into(),
        );
        let constant204: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear2 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            constant148,
            constant149,
            constant1,
            layernormalization1,
            linear1,
            constant172,
            constant166,
            constant204,
            linear2,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        input_ids: Tensor<2, Int>,
        attention_mask: Tensor<2, Int>,
    ) -> (Tensor<3>, i64, Tensor<3>, Tensor<4>, Tensor<4>, Tensor<4>, Tensor<4>) {
        let shape1_out1: [i64; 2] = {
            let axes = &input_ids.clone().dims()[0..2];
            let mut output = [0i64; 2];
            for i in 0..2 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather1_out1 = shape1_out1[1] as i64;
        let range1_out1 = {
            let start = 0i64;
            let delta = 1i64;
            assert!(delta != 0);
            let n = (((gather1_out1 as i128 - start as i128) as f64) / delta as f64)
                .ceil()
                .max(0.0) as i64;
            Tensor::arange(0..n, &self.device)
                .cast(burn::tensor::DType::I64)
                .mul_scalar(delta)
                .add_scalar(start)
        };
        let unsqueeze1_out1: Tensor<2, Int> = range1_out1.unsqueeze_dims::<2>(&[0]);
        let shape2_out1: [i64; 2] = {
            let axes = &attention_mask.clone().dims()[0..2];
            let mut output = [0i64; 2];
            for i in 0..2 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather2_out1 = shape2_out1[0] as i64;
        let gather3_out1 = shape2_out1[1] as i64;
        let unsqueeze2_out1: Tensor<3, Int> = attention_mask.unsqueeze_dims::<3>(&[1]);
        let unsqueeze3_out1: Tensor<4, Int> = unsqueeze2_out1.unsqueeze_dims::<4>(&[2]);
        let unsqueeze4_out1 = [gather2_out1 as i64];
        let unsqueeze5_out1 = [gather3_out1 as i64];
        let unsqueeze6_out1 = [gather3_out1 as i64];
        let constant144_out1: [i64; 1] = [1i64];
        let concat1_out1: [i64; 4usize] = [
            &unsqueeze4_out1[..],
            &constant144_out1[..],
            &unsqueeze5_out1[..],
            &unsqueeze6_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape1_out1 = concat1_out1;
        let shape4_out1: [i64; 1] = [4i64];
        let constantofshape1_out1 = Tensor::<
            1,
            Int,
        >::from_data(
                burn::tensor::TensorData::from([1i64 as i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .reshape([1])
            .expand(shape4_out1);
        let constant148_out1 = self.constant148.val();
        let mul1_out1 = constantofshape1_out1.clone().mul(constant148_out1);
        let equal1_out1 = Tensor::<
            1,
            burn::tensor::Int,
        >::from_data(
                burn::tensor::TensorData::from(&reshape1_out1 as &[i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .equal(mul1_out1);
        let where1_out1 = Tensor::<
            1,
            burn::tensor::Int,
        >::from_data(
                burn::tensor::TensorData::from(&reshape1_out1 as &[i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .mask_where(equal1_out1, constantofshape1_out1);
        let expand1_out1 = {
            let onnx_shape: [i64; 4usize] = TryInto::<
                [i64; 4usize],
            >::try_into(where1_out1.to_data().convert::<i64>().as_slice().unwrap())
                .unwrap();
            let input_dims = unsqueeze3_out1.dims();
            let mut shape = onnx_shape;
            #[allow(clippy::needless_range_loop)]
            for i in 0..4usize {
                let dim_offset = i;
                if shape[dim_offset] == 1 && input_dims[i] > 1 {
                    shape[dim_offset] = input_dims[i] as i64;
                }
            }
            unsqueeze3_out1.expand(shape)
        };
        let cast2_out1 = expand1_out1.float().cast(burn::tensor::DType::F32);
        let constant149_out1 = self.constant149.val();
        let sub1_out1 = (constant149_out1)
            .unsqueeze_dims(&[0isize, 1isize, 2isize])
            .sub(cast2_out1);
        let cast3_out1 = sub1_out1.clone().bool();
        let constant150_out1 = -340282350000000000000000000000000000000f32;
        let where2_out1 = sub1_out1.mask_fill(cast3_out1, constant150_out1);
        let shape5_out1: [i64; 4] = {
            let axes = &where2_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather4_out1 = shape5_out1[2] as i64;
        let range2_out1 = {
            let start = 0i64;
            let delta = 1i64;
            assert!(delta != 0);
            let n = (((gather4_out1 as i128 - start as i128) as f64) / delta as f64)
                .ceil()
                .max(0.0) as i64;
            Tensor::arange(0..n, &self.device)
                .cast(burn::tensor::DType::I64)
                .mul_scalar(delta)
                .add_scalar(start)
        };
        let unsqueeze7_out1: Tensor<2, Int> = range2_out1.unsqueeze_dims::<2>(&[0]);
        let transpose1_out1 = unsqueeze7_out1.clone().permute([1, 0]);
        let sub2_out1 = unsqueeze7_out1.sub(transpose1_out1);
        let abs1_out1 = sub2_out1.abs();
        let constant155_out1 = 64i64;
        let lessorequal1_out1 = abs1_out1.lower_equal_elem(constant155_out1);
        let unsqueeze8_out1: Tensor<3, Bool> = lessorequal1_out1
            .unsqueeze_dims::<3>(&[0]);
        let unsqueeze9_out1: Tensor<4, Bool> = unsqueeze8_out1.unsqueeze_dims::<4>(&[0]);
        let not1_out1 = unsqueeze9_out1.bool_not();
        let constant158_out1 = -340282350000000000000000000000000000000f32;
        let where3_out1 = where2_out1.clone().mask_fill(not1_out1, constant158_out1);
        let constant1_out1 = self.constant1.val();
        let gather5_out1 = {
            let axis_size = constant1_out1.dims()[0] as i64;
            let negative = input_ids.clone().lower_elem(0i64);
            let corrected = input_ids.clone() + axis_size;
            let indices = input_ids.mask_where(negative, corrected);
            constant1_out1.take::<2, 3>(0, indices)
        };
        let layernormalization1_out1 = {
            let dtype = gather5_out1.dtype();
            self.layernormalization1
                .forward(gather5_out1.cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear1_out1 = self.linear1.forward(layernormalization1_out1.clone());
        let shape6_out1: [i64; 3] = {
            let axes = &layernormalization1_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather6_out1 = shape6_out1[0] as i64;
        let unsqueeze10_out1 = [gather6_out1 as i64];
        let constant165_out1: [i64; 1] = [64i64];
        let constant162_out1: [i64; 1] = [-1i64];
        let constant163_out1: [i64; 1] = [3i64];
        let constant164_out1: [i64; 1] = [12i64];
        let concat2_out1: [i64; 5usize] = [
            &unsqueeze10_out1[..],
            &constant162_out1[..],
            &constant163_out1[..],
            &constant164_out1[..],
            &constant165_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape2_out1 = linear1_out1.reshape(concat2_out1);
        let shape7_out1: [i64; 2] = {
            let axes = &unsqueeze1_out1.clone().dims()[0..2];
            let mut output = [0i64; 2];
            for i in 0..2 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather7_out1 = shape7_out1[0] as i64;
        let unsqueeze11_out1 = [gather7_out1 as i64];
        let constant170_out1: [i64; 1] = [1i64];
        let constant169_out1: [i64; 1] = [-1i64];
        let concat3_out1: [i64; 3usize] = [
            &unsqueeze11_out1[..],
            &constant169_out1[..],
            &constant170_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape3_out1 = concat3_out1;
        let shape8_out1: [i64; 1] = [3i64];
        let constantofshape2_out1 = Tensor::<
            1,
            Int,
        >::from_data(
                burn::tensor::TensorData::from([1i64 as i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .reshape([1])
            .expand(shape8_out1);
        let constant172_out1 = self.constant172.val();
        let mul2_out1 = constantofshape2_out1.clone().mul(constant172_out1);
        let equal2_out1 = Tensor::<
            1,
            burn::tensor::Int,
        >::from_data(
                burn::tensor::TensorData::from(&reshape3_out1 as &[i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .equal(mul2_out1);
        let where4_out1 = Tensor::<
            1,
            burn::tensor::Int,
        >::from_data(
                burn::tensor::TensorData::from(&reshape3_out1 as &[i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .mask_where(equal2_out1, constantofshape2_out1);
        let constant166_out1 = self.constant166.val();
        let expand2_out1 = {
            let onnx_shape: [i64; 3usize] = TryInto::<
                [i64; 3usize],
            >::try_into(where4_out1.to_data().convert::<i64>().as_slice().unwrap())
                .unwrap();
            let input_dims = constant166_out1.dims();
            let mut shape = onnx_shape;
            #[allow(clippy::needless_range_loop)]
            for i in 0..3usize {
                let dim_offset = i;
                if shape[dim_offset] == 1 && input_dims[i] > 1 {
                    shape[dim_offset] = input_dims[i] as i64;
                }
            }
            constant166_out1.expand(shape)
        };
        let unsqueeze12_out1: Tensor<3, Int> = unsqueeze1_out1.unsqueeze_dims::<3>(&[1]);
        let cast10_out1 = unsqueeze12_out1.float().cast(burn::tensor::DType::F32);
        let matmul2_out1 = expand2_out1.matmul(cast10_out1.clone());
        let transpose2_out1 = matmul2_out1.permute([0, 2, 1]);
        let concat4_out1 = burn::tensor::Tensor::cat(
            [transpose2_out1.clone(), transpose2_out1].into(),
            2,
        );
        let cos1_out1 = concat4_out1.clone().cos();
        let sin1_out1 = concat4_out1.sin();
        let transpose3_out1 = reshape2_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose3_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split1_out1, split1_out2, split1_out3] = split_tensors.try_into().unwrap();
        let squeeze1_out1 = split1_out1.squeeze_dims::<4>(&[2]);
        let squeeze2_out1 = split1_out2.squeeze_dims::<4>(&[2]);
        let squeeze3_out1 = split1_out3.squeeze_dims::<4>(&[2]);
        let unsqueeze13_out1: Tensor<4> = cos1_out1.unsqueeze_dims::<4>(&[1]);
        let unsqueeze14_out1: Tensor<4> = sin1_out1.unsqueeze_dims::<4>(&[1]);
        let mul5_out1 = squeeze1_out1.clone().mul(unsqueeze13_out1.clone());
        let shape9_out1: [i64; 4] = {
            let axes = &squeeze1_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather8_out1 = shape9_out1[3] as i64;
        let constant183_out1 = 2i64;
        let div1_out1 = gather8_out1 / constant183_out1;
        let unsqueeze15_out1 = [div1_out1 as i64];
        let slice1_out1 = squeeze1_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze15_out1[0]]);
        let unsqueeze16_out1 = [div1_out1 as i64];
        let slice2_out1 = squeeze1_out1
            .slice(s![.., .., .., unsqueeze16_out1[0]..9223372036854775807]);
        let neg1_out1 = slice2_out1.neg();
        let concat5_out1 = burn::tensor::Tensor::cat([neg1_out1, slice1_out1].into(), 3);
        let mul6_out1 = concat5_out1.mul(unsqueeze14_out1.clone());
        let add1_out1 = mul5_out1.add(mul6_out1);
        let mul7_out1 = squeeze2_out1.clone().mul(unsqueeze13_out1.clone());
        let shape10_out1: [i64; 4] = {
            let axes = &squeeze2_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather9_out1 = shape10_out1[3] as i64;
        let constant193_out1 = 2i64;
        let div2_out1 = gather9_out1 / constant193_out1;
        let unsqueeze17_out1 = [div2_out1 as i64];
        let slice3_out1 = squeeze2_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze17_out1[0]]);
        let unsqueeze18_out1 = [div2_out1 as i64];
        let slice4_out1 = squeeze2_out1
            .slice(s![.., .., .., unsqueeze18_out1[0]..9223372036854775807]);
        let neg2_out1 = slice4_out1.neg();
        let concat6_out1 = burn::tensor::Tensor::cat([neg2_out1, slice3_out1].into(), 3);
        let mul8_out1 = concat6_out1.mul(unsqueeze14_out1.clone());
        let add2_out1 = mul7_out1.add(mul8_out1);
        let shape11_out1: [i64; 4] = {
            let axes = &add1_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice5_out1: [i64; 1] = shape11_out1[3..4].try_into().unwrap();
        let cast19_out1 = {
            let shape_array = slice5_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt1_out1 = cast19_out1.sqrt();
        let constant204_out1 = self.constant204.val();
        let div3_out1 = constant204_out1.div(sqrt1_out1);
        let transpose4_out1 = add2_out1.permute([0, 1, 3, 2]);
        let sqrt2_out1 = div3_out1.sqrt();
        let mul9_out1 = add1_out1
            .mul((sqrt2_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul10_out1 = transpose4_out1
            .mul((sqrt2_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul3_out1 = mul9_out1.matmul(mul10_out1);
        let add3_out1 = matmul3_out1.add(where2_out1.clone());
        let softmax1_out1 = burn::tensor::activation::softmax(add3_out1, 3);
        let matmul4_out1 = softmax1_out1.matmul(squeeze3_out1);
        let transpose5_out1 = matmul4_out1.permute([0, 2, 1, 3]);
        let unsqueeze19_out1 = [gather6_out1 as i64];
        let constant207_out1: [i64; 1] = [768i64];
        let constant206_out1: [i64; 1] = [-1i64];
        let concat7_out1: [i64; 3usize] = [
            &unsqueeze19_out1[..],
            &constant206_out1[..],
            &constant207_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape4_out1 = transpose5_out1.reshape(concat7_out1);
        let linear2_out1 = self.linear2.forward(reshape4_out1);
        let add4_out1 = layernormalization1_out1.add(linear2_out1);
        (
            add4_out1,
            gather7_out1,
            cast10_out1,
            where3_out1,
            unsqueeze13_out1,
            unsqueeze14_out1,
            where2_out1,
        )
    }
}
#[derive(Module, Debug)]
pub struct Submodule2 {
    layernormalization2: LayerNorm,
    linear3: Linear,
    constant215: burn::module::Param<Tensor<1>>,
    constant216: burn::module::Param<Tensor<1>>,
    constant217: burn::module::Param<Tensor<1>>,
    linear4: Linear,
    layernormalization3: LayerNorm,
    linear5: Linear,
    constant230: burn::module::Param<Tensor<1, Int>>,
    constant225: burn::module::Param<Tensor<3>>,
    constant261: burn::module::Param<Tensor<1>>,
    linear6: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule2 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization2 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear3 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant215: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant216: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant217: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear4 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization3 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear5 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant230: burn::module::Param<Tensor<1, Int>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
                Int,
            >::from_data(
                burn::tensor::TensorData::from([-1i64]),
                (device, burn::tensor::DType::I64),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant225: burn::module::Param<Tensor<3>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                3,
            >::zeros([1, 32, 1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1, 32, 1].into(),
        );
        let constant261: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear6 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization2,
            linear3,
            constant215,
            constant216,
            constant217,
            linear4,
            layernormalization3,
            linear5,
            constant230,
            constant225,
            constant261,
            linear6,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add4_out1: Tensor<3>,
        gather7_out1: i64,
        cast10_out1: Tensor<3>,
        where3_out1: Tensor<4>,
    ) -> (Tensor<3>, Tensor<4>, Tensor<4>) {
        let layernormalization2_out1 = {
            let dtype = add4_out1.clone().dtype();
            self.layernormalization2
                .forward(add4_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear3_out1 = self.linear3.forward(layernormalization2_out1);
        let shape12_out1: [i64; 3] = {
            let axes = &linear3_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant209_out1: [i64; 1] = [-1i64];
        let gather10_out1: [i64; 1usize] = constant209_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape12_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape12_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant211_out1: [i64; 1] = [1i64];
        let add5_out1 = {
            let __lhs = gather10_out1;
            let __rhs = constant211_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant212_out1: [i64; 1] = [2i64];
        let div4_out1 = {
            let __lhs = add5_out1;
            let __rhs = constant212_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice6_out1 = linear3_out1.clone().slice(s![.., .., 0..div4_out1[0]]);
        let constant214_out1: [i64; 1] = [2i64];
        let mul12_out1 = {
            let __lhs = div4_out1;
            let __rhs = constant214_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice7_out1 = linear3_out1.slice(s![.., .., div4_out1[0]..mul12_out1[0]]);
        let constant215_out1 = self.constant215.val();
        let div5_out1 = slice6_out1
            .clone()
            .div((constant215_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf1_out1 = div5_out1.erf();
        let constant216_out1 = self.constant216.val();
        let add6_out1 = erf1_out1
            .add((constant216_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul13_out1 = slice6_out1.mul(add6_out1);
        let constant217_out1 = self.constant217.val();
        let mul14_out1 = mul13_out1
            .mul((constant217_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul15_out1 = mul14_out1.mul(slice7_out1);
        let linear4_out1 = self.linear4.forward(mul15_out1);
        let add7_out1 = add4_out1.add(linear4_out1);
        let layernormalization3_out1 = {
            let dtype = add7_out1.clone().dtype();
            self.layernormalization3
                .forward(add7_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear5_out1 = self.linear5.forward(layernormalization3_out1.clone());
        let shape13_out1: [i64; 3] = {
            let axes = &layernormalization3_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather11_out1 = shape13_out1[0] as i64;
        let unsqueeze20_out1 = [gather11_out1 as i64];
        let constant224_out1: [i64; 1] = [64i64];
        let constant221_out1: [i64; 1] = [-1i64];
        let constant222_out1: [i64; 1] = [3i64];
        let constant223_out1: [i64; 1] = [12i64];
        let concat8_out1: [i64; 5usize] = [
            &unsqueeze20_out1[..],
            &constant221_out1[..],
            &constant222_out1[..],
            &constant223_out1[..],
            &constant224_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape5_out1 = linear5_out1.reshape(concat8_out1);
        let unsqueeze21_out1 = [gather7_out1 as i64];
        let constant228_out1: [i64; 1] = [1i64];
        let constant227_out1: [i64; 1] = [-1i64];
        let concat9_out1: [i64; 3usize] = [
            &unsqueeze21_out1[..],
            &constant227_out1[..],
            &constant228_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape6_out1 = concat9_out1;
        let shape14_out1: [i64; 1] = [3i64];
        let constantofshape3_out1 = Tensor::<
            1,
            Int,
        >::from_data(
                burn::tensor::TensorData::from([1i64 as i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .reshape([1])
            .expand(shape14_out1);
        let constant230_out1 = self.constant230.val();
        let mul16_out1 = constantofshape3_out1.clone().mul(constant230_out1);
        let equal3_out1 = Tensor::<
            1,
            burn::tensor::Int,
        >::from_data(
                burn::tensor::TensorData::from(&reshape6_out1 as &[i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .equal(mul16_out1);
        let where5_out1 = Tensor::<
            1,
            burn::tensor::Int,
        >::from_data(
                burn::tensor::TensorData::from(&reshape6_out1 as &[i64]),
                (&self.device, burn::tensor::DType::I64),
            )
            .mask_where(equal3_out1, constantofshape3_out1);
        let constant225_out1 = self.constant225.val();
        let expand3_out1 = {
            let onnx_shape: [i64; 3usize] = TryInto::<
                [i64; 3usize],
            >::try_into(where5_out1.to_data().convert::<i64>().as_slice().unwrap())
                .unwrap();
            let input_dims = constant225_out1.dims();
            let mut shape = onnx_shape;
            #[allow(clippy::needless_range_loop)]
            for i in 0..3usize {
                let dim_offset = i;
                if shape[dim_offset] == 1 && input_dims[i] > 1 {
                    shape[dim_offset] = input_dims[i] as i64;
                }
            }
            constant225_out1.expand(shape)
        };
        let matmul9_out1 = expand3_out1.matmul(cast10_out1);
        let transpose6_out1 = matmul9_out1.permute([0, 2, 1]);
        let concat10_out1 = burn::tensor::Tensor::cat(
            [transpose6_out1.clone(), transpose6_out1].into(),
            2,
        );
        let cos2_out1 = concat10_out1.clone().cos();
        let sin2_out1 = concat10_out1.sin();
        let transpose7_out1 = reshape5_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose7_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split2_out1, split2_out2, split2_out3] = split_tensors.try_into().unwrap();
        let squeeze4_out1 = split2_out1.squeeze_dims::<4>(&[2]);
        let squeeze5_out1 = split2_out2.squeeze_dims::<4>(&[2]);
        let squeeze6_out1 = split2_out3.squeeze_dims::<4>(&[2]);
        let unsqueeze22_out1: Tensor<4> = cos2_out1.unsqueeze_dims::<4>(&[1]);
        let unsqueeze23_out1: Tensor<4> = sin2_out1.unsqueeze_dims::<4>(&[1]);
        let mul19_out1 = squeeze4_out1.clone().mul(unsqueeze22_out1.clone());
        let shape15_out1: [i64; 4] = {
            let axes = &squeeze4_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather12_out1 = shape15_out1[3] as i64;
        let constant240_out1 = 2i64;
        let div6_out1 = gather12_out1 / constant240_out1;
        let unsqueeze24_out1 = [div6_out1 as i64];
        let slice8_out1 = squeeze4_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze24_out1[0]]);
        let unsqueeze25_out1 = [div6_out1 as i64];
        let slice9_out1 = squeeze4_out1
            .slice(s![.., .., .., unsqueeze25_out1[0]..9223372036854775807]);
        let neg3_out1 = slice9_out1.neg();
        let concat11_out1 = burn::tensor::Tensor::cat(
            [neg3_out1, slice8_out1].into(),
            3,
        );
        let mul20_out1 = concat11_out1.mul(unsqueeze23_out1.clone());
        let add8_out1 = mul19_out1.add(mul20_out1);
        let mul21_out1 = squeeze5_out1.clone().mul(unsqueeze22_out1.clone());
        let shape16_out1: [i64; 4] = {
            let axes = &squeeze5_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather13_out1 = shape16_out1[3] as i64;
        let constant250_out1 = 2i64;
        let div7_out1 = gather13_out1 / constant250_out1;
        let unsqueeze26_out1 = [div7_out1 as i64];
        let slice10_out1 = squeeze5_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze26_out1[0]]);
        let unsqueeze27_out1 = [div7_out1 as i64];
        let slice11_out1 = squeeze5_out1
            .slice(s![.., .., .., unsqueeze27_out1[0]..9223372036854775807]);
        let neg4_out1 = slice11_out1.neg();
        let concat12_out1 = burn::tensor::Tensor::cat(
            [neg4_out1, slice10_out1].into(),
            3,
        );
        let mul22_out1 = concat12_out1.mul(unsqueeze23_out1.clone());
        let add9_out1 = mul21_out1.add(mul22_out1);
        let shape17_out1: [i64; 4] = {
            let axes = &add8_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice12_out1: [i64; 1] = shape17_out1[3..4].try_into().unwrap();
        let cast29_out1 = {
            let shape_array = slice12_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt4_out1 = cast29_out1.sqrt();
        let constant261_out1 = self.constant261.val();
        let div8_out1 = constant261_out1.div(sqrt4_out1);
        let transpose8_out1 = add9_out1.permute([0, 1, 3, 2]);
        let sqrt5_out1 = div8_out1.sqrt();
        let mul23_out1 = add8_out1
            .mul((sqrt5_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul24_out1 = transpose8_out1
            .mul((sqrt5_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul10_out1 = mul23_out1.matmul(mul24_out1);
        let add10_out1 = matmul10_out1.add(where3_out1);
        let softmax2_out1 = burn::tensor::activation::softmax(add10_out1, 3);
        let matmul11_out1 = softmax2_out1.matmul(squeeze6_out1);
        let transpose9_out1 = matmul11_out1.permute([0, 2, 1, 3]);
        let unsqueeze28_out1 = [gather11_out1 as i64];
        let constant264_out1: [i64; 1] = [768i64];
        let constant263_out1: [i64; 1] = [-1i64];
        let concat13_out1: [i64; 3usize] = [
            &unsqueeze28_out1[..],
            &constant263_out1[..],
            &constant264_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape7_out1 = transpose9_out1.reshape(concat13_out1);
        let linear6_out1 = self.linear6.forward(reshape7_out1);
        let add11_out1 = add7_out1.add(linear6_out1);
        (add11_out1, unsqueeze22_out1, unsqueeze23_out1)
    }
}
#[derive(Module, Debug)]
pub struct Submodule3 {
    layernormalization4: LayerNorm,
    linear7: Linear,
    constant272: burn::module::Param<Tensor<1>>,
    constant273: burn::module::Param<Tensor<1>>,
    constant274: burn::module::Param<Tensor<1>>,
    linear8: Linear,
    layernormalization5: LayerNorm,
    linear9: Linear,
    constant308: burn::module::Param<Tensor<1>>,
    linear10: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule3 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization4 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear7 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant272: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant273: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant274: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear8 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization5 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear9 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant308: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear10 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization4,
            linear7,
            constant272,
            constant273,
            constant274,
            linear8,
            layernormalization5,
            linear9,
            constant308,
            linear10,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add11_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization4_out1 = {
            let dtype = add11_out1.clone().dtype();
            self.layernormalization4
                .forward(add11_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear7_out1 = self.linear7.forward(layernormalization4_out1);
        let shape18_out1: [i64; 3] = {
            let axes = &linear7_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant266_out1: [i64; 1] = [-1i64];
        let gather14_out1: [i64; 1usize] = constant266_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape18_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape18_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant268_out1: [i64; 1] = [1i64];
        let add12_out1 = {
            let __lhs = gather14_out1;
            let __rhs = constant268_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant269_out1: [i64; 1] = [2i64];
        let div9_out1 = {
            let __lhs = add12_out1;
            let __rhs = constant269_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice13_out1 = linear7_out1.clone().slice(s![.., .., 0..div9_out1[0]]);
        let constant271_out1: [i64; 1] = [2i64];
        let mul26_out1 = {
            let __lhs = div9_out1;
            let __rhs = constant271_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice14_out1 = linear7_out1.slice(s![.., .., div9_out1[0]..mul26_out1[0]]);
        let constant272_out1 = self.constant272.val();
        let div10_out1 = slice13_out1
            .clone()
            .div((constant272_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf2_out1 = div10_out1.erf();
        let constant273_out1 = self.constant273.val();
        let add13_out1 = erf2_out1
            .add((constant273_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul27_out1 = slice13_out1.mul(add13_out1);
        let constant274_out1 = self.constant274.val();
        let mul28_out1 = mul27_out1
            .mul((constant274_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul29_out1 = mul28_out1.mul(slice14_out1);
        let linear8_out1 = self.linear8.forward(mul29_out1);
        let add14_out1 = add11_out1.add(linear8_out1);
        let layernormalization5_out1 = {
            let dtype = add14_out1.clone().dtype();
            self.layernormalization5
                .forward(add14_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear9_out1 = self.linear9.forward(layernormalization5_out1.clone());
        let shape19_out1: [i64; 3] = {
            let axes = &layernormalization5_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather15_out1 = shape19_out1[0] as i64;
        let unsqueeze29_out1 = [gather15_out1 as i64];
        let constant281_out1: [i64; 1] = [64i64];
        let constant278_out1: [i64; 1] = [-1i64];
        let constant279_out1: [i64; 1] = [3i64];
        let constant280_out1: [i64; 1] = [12i64];
        let concat14_out1: [i64; 5usize] = [
            &unsqueeze29_out1[..],
            &constant278_out1[..],
            &constant279_out1[..],
            &constant280_out1[..],
            &constant281_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape8_out1 = linear9_out1.reshape(concat14_out1);
        let transpose10_out1 = reshape8_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose10_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split3_out1, split3_out2, split3_out3] = split_tensors.try_into().unwrap();
        let squeeze7_out1 = split3_out1.squeeze_dims::<4>(&[2]);
        let squeeze8_out1 = split3_out2.squeeze_dims::<4>(&[2]);
        let squeeze9_out1 = split3_out3.squeeze_dims::<4>(&[2]);
        let mul30_out1 = squeeze7_out1.clone().mul(unsqueeze22_out1.clone());
        let shape20_out1: [i64; 4] = {
            let axes = &squeeze7_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather16_out1 = shape20_out1[3] as i64;
        let constant287_out1 = 2i64;
        let div11_out1 = gather16_out1 / constant287_out1;
        let unsqueeze30_out1 = [div11_out1 as i64];
        let slice15_out1 = squeeze7_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze30_out1[0]]);
        let unsqueeze31_out1 = [div11_out1 as i64];
        let slice16_out1 = squeeze7_out1
            .slice(s![.., .., .., unsqueeze31_out1[0]..9223372036854775807]);
        let neg5_out1 = slice16_out1.neg();
        let concat15_out1 = burn::tensor::Tensor::cat(
            [neg5_out1, slice15_out1].into(),
            3,
        );
        let mul31_out1 = concat15_out1.mul(unsqueeze23_out1.clone());
        let add15_out1 = mul30_out1.add(mul31_out1);
        let mul32_out1 = squeeze8_out1.clone().mul(unsqueeze22_out1);
        let shape21_out1: [i64; 4] = {
            let axes = &squeeze8_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather17_out1 = shape21_out1[3] as i64;
        let constant297_out1 = 2i64;
        let div12_out1 = gather17_out1 / constant297_out1;
        let unsqueeze32_out1 = [div12_out1 as i64];
        let slice17_out1 = squeeze8_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze32_out1[0]]);
        let unsqueeze33_out1 = [div12_out1 as i64];
        let slice18_out1 = squeeze8_out1
            .slice(s![.., .., .., unsqueeze33_out1[0]..9223372036854775807]);
        let neg6_out1 = slice18_out1.neg();
        let concat16_out1 = burn::tensor::Tensor::cat(
            [neg6_out1, slice17_out1].into(),
            3,
        );
        let mul33_out1 = concat16_out1.mul(unsqueeze23_out1);
        let add16_out1 = mul32_out1.add(mul33_out1);
        let shape22_out1: [i64; 4] = {
            let axes = &add15_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice19_out1: [i64; 1] = shape22_out1[3..4].try_into().unwrap();
        let cast35_out1 = {
            let shape_array = slice19_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt7_out1 = cast35_out1.sqrt();
        let constant308_out1 = self.constant308.val();
        let div13_out1 = constant308_out1.div(sqrt7_out1);
        let transpose11_out1 = add16_out1.permute([0, 1, 3, 2]);
        let sqrt8_out1 = div13_out1.sqrt();
        let mul34_out1 = add15_out1
            .mul((sqrt8_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul35_out1 = transpose11_out1
            .mul((sqrt8_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul16_out1 = mul34_out1.matmul(mul35_out1);
        let add17_out1 = matmul16_out1.add(where3_out1);
        let softmax3_out1 = burn::tensor::activation::softmax(add17_out1, 3);
        let matmul17_out1 = softmax3_out1.matmul(squeeze9_out1);
        let transpose12_out1 = matmul17_out1.permute([0, 2, 1, 3]);
        let unsqueeze34_out1 = [gather15_out1 as i64];
        let constant311_out1: [i64; 1] = [768i64];
        let constant310_out1: [i64; 1] = [-1i64];
        let concat17_out1: [i64; 3usize] = [
            &unsqueeze34_out1[..],
            &constant310_out1[..],
            &constant311_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape9_out1 = transpose12_out1.reshape(concat17_out1);
        let linear10_out1 = self.linear10.forward(reshape9_out1);
        let add18_out1 = add14_out1.add(linear10_out1);
        add18_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule4 {
    layernormalization6: LayerNorm,
    linear11: Linear,
    constant319: burn::module::Param<Tensor<1>>,
    constant320: burn::module::Param<Tensor<1>>,
    constant321: burn::module::Param<Tensor<1>>,
    linear12: Linear,
    layernormalization7: LayerNorm,
    linear13: Linear,
    constant355: burn::module::Param<Tensor<1>>,
    linear14: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule4 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization6 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear11 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant319: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant320: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant321: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear12 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization7 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear13 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant355: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear14 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization6,
            linear11,
            constant319,
            constant320,
            constant321,
            linear12,
            layernormalization7,
            linear13,
            constant355,
            linear14,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add18_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization6_out1 = {
            let dtype = add18_out1.clone().dtype();
            self.layernormalization6
                .forward(add18_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear11_out1 = self.linear11.forward(layernormalization6_out1);
        let shape23_out1: [i64; 3] = {
            let axes = &linear11_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant313_out1: [i64; 1] = [-1i64];
        let gather18_out1: [i64; 1usize] = constant313_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape23_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape23_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant315_out1: [i64; 1] = [1i64];
        let add19_out1 = {
            let __lhs = gather18_out1;
            let __rhs = constant315_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant316_out1: [i64; 1] = [2i64];
        let div14_out1 = {
            let __lhs = add19_out1;
            let __rhs = constant316_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice20_out1 = linear11_out1.clone().slice(s![.., .., 0..div14_out1[0]]);
        let constant318_out1: [i64; 1] = [2i64];
        let mul37_out1 = {
            let __lhs = div14_out1;
            let __rhs = constant318_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice21_out1 = linear11_out1.slice(s![.., .., div14_out1[0]..mul37_out1[0]]);
        let constant319_out1 = self.constant319.val();
        let div15_out1 = slice20_out1
            .clone()
            .div((constant319_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf3_out1 = div15_out1.erf();
        let constant320_out1 = self.constant320.val();
        let add20_out1 = erf3_out1
            .add((constant320_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul38_out1 = slice20_out1.mul(add20_out1);
        let constant321_out1 = self.constant321.val();
        let mul39_out1 = mul38_out1
            .mul((constant321_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul40_out1 = mul39_out1.mul(slice21_out1);
        let linear12_out1 = self.linear12.forward(mul40_out1);
        let add21_out1 = add18_out1.add(linear12_out1);
        let layernormalization7_out1 = {
            let dtype = add21_out1.clone().dtype();
            self.layernormalization7
                .forward(add21_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear13_out1 = self.linear13.forward(layernormalization7_out1.clone());
        let shape24_out1: [i64; 3] = {
            let axes = &layernormalization7_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather19_out1 = shape24_out1[0] as i64;
        let unsqueeze35_out1 = [gather19_out1 as i64];
        let constant328_out1: [i64; 1] = [64i64];
        let constant325_out1: [i64; 1] = [-1i64];
        let constant326_out1: [i64; 1] = [3i64];
        let constant327_out1: [i64; 1] = [12i64];
        let concat18_out1: [i64; 5usize] = [
            &unsqueeze35_out1[..],
            &constant325_out1[..],
            &constant326_out1[..],
            &constant327_out1[..],
            &constant328_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape10_out1 = linear13_out1.reshape(concat18_out1);
        let transpose13_out1 = reshape10_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose13_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split4_out1, split4_out2, split4_out3] = split_tensors.try_into().unwrap();
        let squeeze10_out1 = split4_out1.squeeze_dims::<4>(&[2]);
        let squeeze11_out1 = split4_out2.squeeze_dims::<4>(&[2]);
        let squeeze12_out1 = split4_out3.squeeze_dims::<4>(&[2]);
        let mul41_out1 = squeeze10_out1.clone().mul(unsqueeze13_out1.clone());
        let shape25_out1: [i64; 4] = {
            let axes = &squeeze10_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather20_out1 = shape25_out1[3] as i64;
        let constant334_out1 = 2i64;
        let div16_out1 = gather20_out1 / constant334_out1;
        let unsqueeze36_out1 = [div16_out1 as i64];
        let slice22_out1 = squeeze10_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze36_out1[0]]);
        let unsqueeze37_out1 = [div16_out1 as i64];
        let slice23_out1 = squeeze10_out1
            .slice(s![.., .., .., unsqueeze37_out1[0]..9223372036854775807]);
        let neg7_out1 = slice23_out1.neg();
        let concat19_out1 = burn::tensor::Tensor::cat(
            [neg7_out1, slice22_out1].into(),
            3,
        );
        let mul42_out1 = concat19_out1.mul(unsqueeze14_out1.clone());
        let add22_out1 = mul41_out1.add(mul42_out1);
        let mul43_out1 = squeeze11_out1.clone().mul(unsqueeze13_out1);
        let shape26_out1: [i64; 4] = {
            let axes = &squeeze11_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather21_out1 = shape26_out1[3] as i64;
        let constant344_out1 = 2i64;
        let div17_out1 = gather21_out1 / constant344_out1;
        let unsqueeze38_out1 = [div17_out1 as i64];
        let slice24_out1 = squeeze11_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze38_out1[0]]);
        let unsqueeze39_out1 = [div17_out1 as i64];
        let slice25_out1 = squeeze11_out1
            .slice(s![.., .., .., unsqueeze39_out1[0]..9223372036854775807]);
        let neg8_out1 = slice25_out1.neg();
        let concat20_out1 = burn::tensor::Tensor::cat(
            [neg8_out1, slice24_out1].into(),
            3,
        );
        let mul44_out1 = concat20_out1.mul(unsqueeze14_out1);
        let add23_out1 = mul43_out1.add(mul44_out1);
        let shape27_out1: [i64; 4] = {
            let axes = &add22_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice26_out1: [i64; 1] = shape27_out1[3..4].try_into().unwrap();
        let cast41_out1 = {
            let shape_array = slice26_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt10_out1 = cast41_out1.sqrt();
        let constant355_out1 = self.constant355.val();
        let div18_out1 = constant355_out1.div(sqrt10_out1);
        let transpose14_out1 = add23_out1.permute([0, 1, 3, 2]);
        let sqrt11_out1 = div18_out1.sqrt();
        let mul45_out1 = add22_out1
            .mul((sqrt11_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul46_out1 = transpose14_out1
            .mul((sqrt11_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul22_out1 = mul45_out1.matmul(mul46_out1);
        let add24_out1 = matmul22_out1.add(where2_out1);
        let softmax4_out1 = burn::tensor::activation::softmax(add24_out1, 3);
        let matmul23_out1 = softmax4_out1.matmul(squeeze12_out1);
        let transpose15_out1 = matmul23_out1.permute([0, 2, 1, 3]);
        let unsqueeze40_out1 = [gather19_out1 as i64];
        let constant358_out1: [i64; 1] = [768i64];
        let constant357_out1: [i64; 1] = [-1i64];
        let concat21_out1: [i64; 3usize] = [
            &unsqueeze40_out1[..],
            &constant357_out1[..],
            &constant358_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape11_out1 = transpose15_out1.reshape(concat21_out1);
        let linear14_out1 = self.linear14.forward(reshape11_out1);
        let add25_out1 = add21_out1.add(linear14_out1);
        add25_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule5 {
    layernormalization8: LayerNorm,
    linear15: Linear,
    constant366: burn::module::Param<Tensor<1>>,
    constant367: burn::module::Param<Tensor<1>>,
    constant368: burn::module::Param<Tensor<1>>,
    linear16: Linear,
    layernormalization9: LayerNorm,
    linear17: Linear,
    constant402: burn::module::Param<Tensor<1>>,
    linear18: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule5 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization8 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear15 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant366: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant367: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant368: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear16 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization9 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear17 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant402: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear18 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization8,
            linear15,
            constant366,
            constant367,
            constant368,
            linear16,
            layernormalization9,
            linear17,
            constant402,
            linear18,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add25_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization8_out1 = {
            let dtype = add25_out1.clone().dtype();
            self.layernormalization8
                .forward(add25_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear15_out1 = self.linear15.forward(layernormalization8_out1);
        let shape28_out1: [i64; 3] = {
            let axes = &linear15_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant360_out1: [i64; 1] = [-1i64];
        let gather22_out1: [i64; 1usize] = constant360_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape28_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape28_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant362_out1: [i64; 1] = [1i64];
        let add26_out1 = {
            let __lhs = gather22_out1;
            let __rhs = constant362_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant363_out1: [i64; 1] = [2i64];
        let div19_out1 = {
            let __lhs = add26_out1;
            let __rhs = constant363_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice27_out1 = linear15_out1.clone().slice(s![.., .., 0..div19_out1[0]]);
        let constant365_out1: [i64; 1] = [2i64];
        let mul48_out1 = {
            let __lhs = div19_out1;
            let __rhs = constant365_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice28_out1 = linear15_out1.slice(s![.., .., div19_out1[0]..mul48_out1[0]]);
        let constant366_out1 = self.constant366.val();
        let div20_out1 = slice27_out1
            .clone()
            .div((constant366_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf4_out1 = div20_out1.erf();
        let constant367_out1 = self.constant367.val();
        let add27_out1 = erf4_out1
            .add((constant367_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul49_out1 = slice27_out1.mul(add27_out1);
        let constant368_out1 = self.constant368.val();
        let mul50_out1 = mul49_out1
            .mul((constant368_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul51_out1 = mul50_out1.mul(slice28_out1);
        let linear16_out1 = self.linear16.forward(mul51_out1);
        let add28_out1 = add25_out1.add(linear16_out1);
        let layernormalization9_out1 = {
            let dtype = add28_out1.clone().dtype();
            self.layernormalization9
                .forward(add28_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear17_out1 = self.linear17.forward(layernormalization9_out1.clone());
        let shape29_out1: [i64; 3] = {
            let axes = &layernormalization9_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather23_out1 = shape29_out1[0] as i64;
        let unsqueeze41_out1 = [gather23_out1 as i64];
        let constant375_out1: [i64; 1] = [64i64];
        let constant372_out1: [i64; 1] = [-1i64];
        let constant373_out1: [i64; 1] = [3i64];
        let constant374_out1: [i64; 1] = [12i64];
        let concat22_out1: [i64; 5usize] = [
            &unsqueeze41_out1[..],
            &constant372_out1[..],
            &constant373_out1[..],
            &constant374_out1[..],
            &constant375_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape12_out1 = linear17_out1.reshape(concat22_out1);
        let transpose16_out1 = reshape12_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose16_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split5_out1, split5_out2, split5_out3] = split_tensors.try_into().unwrap();
        let squeeze13_out1 = split5_out1.squeeze_dims::<4>(&[2]);
        let squeeze14_out1 = split5_out2.squeeze_dims::<4>(&[2]);
        let squeeze15_out1 = split5_out3.squeeze_dims::<4>(&[2]);
        let mul52_out1 = squeeze13_out1.clone().mul(unsqueeze22_out1.clone());
        let shape30_out1: [i64; 4] = {
            let axes = &squeeze13_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather24_out1 = shape30_out1[3] as i64;
        let constant381_out1 = 2i64;
        let div21_out1 = gather24_out1 / constant381_out1;
        let unsqueeze42_out1 = [div21_out1 as i64];
        let slice29_out1 = squeeze13_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze42_out1[0]]);
        let unsqueeze43_out1 = [div21_out1 as i64];
        let slice30_out1 = squeeze13_out1
            .slice(s![.., .., .., unsqueeze43_out1[0]..9223372036854775807]);
        let neg9_out1 = slice30_out1.neg();
        let concat23_out1 = burn::tensor::Tensor::cat(
            [neg9_out1, slice29_out1].into(),
            3,
        );
        let mul53_out1 = concat23_out1.mul(unsqueeze23_out1.clone());
        let add29_out1 = mul52_out1.add(mul53_out1);
        let mul54_out1 = squeeze14_out1.clone().mul(unsqueeze22_out1);
        let shape31_out1: [i64; 4] = {
            let axes = &squeeze14_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather25_out1 = shape31_out1[3] as i64;
        let constant391_out1 = 2i64;
        let div22_out1 = gather25_out1 / constant391_out1;
        let unsqueeze44_out1 = [div22_out1 as i64];
        let slice31_out1 = squeeze14_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze44_out1[0]]);
        let unsqueeze45_out1 = [div22_out1 as i64];
        let slice32_out1 = squeeze14_out1
            .slice(s![.., .., .., unsqueeze45_out1[0]..9223372036854775807]);
        let neg10_out1 = slice32_out1.neg();
        let concat24_out1 = burn::tensor::Tensor::cat(
            [neg10_out1, slice31_out1].into(),
            3,
        );
        let mul55_out1 = concat24_out1.mul(unsqueeze23_out1);
        let add30_out1 = mul54_out1.add(mul55_out1);
        let shape32_out1: [i64; 4] = {
            let axes = &add29_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice33_out1: [i64; 1] = shape32_out1[3..4].try_into().unwrap();
        let cast47_out1 = {
            let shape_array = slice33_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt13_out1 = cast47_out1.sqrt();
        let constant402_out1 = self.constant402.val();
        let div23_out1 = constant402_out1.div(sqrt13_out1);
        let transpose17_out1 = add30_out1.permute([0, 1, 3, 2]);
        let sqrt14_out1 = div23_out1.sqrt();
        let mul56_out1 = add29_out1
            .mul((sqrt14_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul57_out1 = transpose17_out1
            .mul((sqrt14_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul28_out1 = mul56_out1.matmul(mul57_out1);
        let add31_out1 = matmul28_out1.add(where3_out1);
        let softmax5_out1 = burn::tensor::activation::softmax(add31_out1, 3);
        let matmul29_out1 = softmax5_out1.matmul(squeeze15_out1);
        let transpose18_out1 = matmul29_out1.permute([0, 2, 1, 3]);
        let unsqueeze46_out1 = [gather23_out1 as i64];
        let constant405_out1: [i64; 1] = [768i64];
        let constant404_out1: [i64; 1] = [-1i64];
        let concat25_out1: [i64; 3usize] = [
            &unsqueeze46_out1[..],
            &constant404_out1[..],
            &constant405_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape13_out1 = transpose18_out1.reshape(concat25_out1);
        let linear18_out1 = self.linear18.forward(reshape13_out1);
        let add32_out1 = add28_out1.add(linear18_out1);
        add32_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule6 {
    layernormalization10: LayerNorm,
    linear19: Linear,
    constant413: burn::module::Param<Tensor<1>>,
    constant414: burn::module::Param<Tensor<1>>,
    constant415: burn::module::Param<Tensor<1>>,
    linear20: Linear,
    layernormalization11: LayerNorm,
    linear21: Linear,
    constant449: burn::module::Param<Tensor<1>>,
    linear22: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule6 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization10 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear19 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant413: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant414: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant415: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear20 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization11 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear21 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant449: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear22 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization10,
            linear19,
            constant413,
            constant414,
            constant415,
            linear20,
            layernormalization11,
            linear21,
            constant449,
            linear22,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add32_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization10_out1 = {
            let dtype = add32_out1.clone().dtype();
            self.layernormalization10
                .forward(add32_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear19_out1 = self.linear19.forward(layernormalization10_out1);
        let shape33_out1: [i64; 3] = {
            let axes = &linear19_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant407_out1: [i64; 1] = [-1i64];
        let gather26_out1: [i64; 1usize] = constant407_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape33_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape33_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant409_out1: [i64; 1] = [1i64];
        let add33_out1 = {
            let __lhs = gather26_out1;
            let __rhs = constant409_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant410_out1: [i64; 1] = [2i64];
        let div24_out1 = {
            let __lhs = add33_out1;
            let __rhs = constant410_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice34_out1 = linear19_out1.clone().slice(s![.., .., 0..div24_out1[0]]);
        let constant412_out1: [i64; 1] = [2i64];
        let mul59_out1 = {
            let __lhs = div24_out1;
            let __rhs = constant412_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice35_out1 = linear19_out1.slice(s![.., .., div24_out1[0]..mul59_out1[0]]);
        let constant413_out1 = self.constant413.val();
        let div25_out1 = slice34_out1
            .clone()
            .div((constant413_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf5_out1 = div25_out1.erf();
        let constant414_out1 = self.constant414.val();
        let add34_out1 = erf5_out1
            .add((constant414_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul60_out1 = slice34_out1.mul(add34_out1);
        let constant415_out1 = self.constant415.val();
        let mul61_out1 = mul60_out1
            .mul((constant415_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul62_out1 = mul61_out1.mul(slice35_out1);
        let linear20_out1 = self.linear20.forward(mul62_out1);
        let add35_out1 = add32_out1.add(linear20_out1);
        let layernormalization11_out1 = {
            let dtype = add35_out1.clone().dtype();
            self.layernormalization11
                .forward(add35_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear21_out1 = self.linear21.forward(layernormalization11_out1.clone());
        let shape34_out1: [i64; 3] = {
            let axes = &layernormalization11_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather27_out1 = shape34_out1[0] as i64;
        let unsqueeze47_out1 = [gather27_out1 as i64];
        let constant422_out1: [i64; 1] = [64i64];
        let constant419_out1: [i64; 1] = [-1i64];
        let constant420_out1: [i64; 1] = [3i64];
        let constant421_out1: [i64; 1] = [12i64];
        let concat26_out1: [i64; 5usize] = [
            &unsqueeze47_out1[..],
            &constant419_out1[..],
            &constant420_out1[..],
            &constant421_out1[..],
            &constant422_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape14_out1 = linear21_out1.reshape(concat26_out1);
        let transpose19_out1 = reshape14_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose19_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split6_out1, split6_out2, split6_out3] = split_tensors.try_into().unwrap();
        let squeeze16_out1 = split6_out1.squeeze_dims::<4>(&[2]);
        let squeeze17_out1 = split6_out2.squeeze_dims::<4>(&[2]);
        let squeeze18_out1 = split6_out3.squeeze_dims::<4>(&[2]);
        let mul63_out1 = squeeze16_out1.clone().mul(unsqueeze22_out1.clone());
        let shape35_out1: [i64; 4] = {
            let axes = &squeeze16_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather28_out1 = shape35_out1[3] as i64;
        let constant428_out1 = 2i64;
        let div26_out1 = gather28_out1 / constant428_out1;
        let unsqueeze48_out1 = [div26_out1 as i64];
        let slice36_out1 = squeeze16_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze48_out1[0]]);
        let unsqueeze49_out1 = [div26_out1 as i64];
        let slice37_out1 = squeeze16_out1
            .slice(s![.., .., .., unsqueeze49_out1[0]..9223372036854775807]);
        let neg11_out1 = slice37_out1.neg();
        let concat27_out1 = burn::tensor::Tensor::cat(
            [neg11_out1, slice36_out1].into(),
            3,
        );
        let mul64_out1 = concat27_out1.mul(unsqueeze23_out1.clone());
        let add36_out1 = mul63_out1.add(mul64_out1);
        let mul65_out1 = squeeze17_out1.clone().mul(unsqueeze22_out1);
        let shape36_out1: [i64; 4] = {
            let axes = &squeeze17_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather29_out1 = shape36_out1[3] as i64;
        let constant438_out1 = 2i64;
        let div27_out1 = gather29_out1 / constant438_out1;
        let unsqueeze50_out1 = [div27_out1 as i64];
        let slice38_out1 = squeeze17_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze50_out1[0]]);
        let unsqueeze51_out1 = [div27_out1 as i64];
        let slice39_out1 = squeeze17_out1
            .slice(s![.., .., .., unsqueeze51_out1[0]..9223372036854775807]);
        let neg12_out1 = slice39_out1.neg();
        let concat28_out1 = burn::tensor::Tensor::cat(
            [neg12_out1, slice38_out1].into(),
            3,
        );
        let mul66_out1 = concat28_out1.mul(unsqueeze23_out1);
        let add37_out1 = mul65_out1.add(mul66_out1);
        let shape37_out1: [i64; 4] = {
            let axes = &add36_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice40_out1: [i64; 1] = shape37_out1[3..4].try_into().unwrap();
        let cast53_out1 = {
            let shape_array = slice40_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt16_out1 = cast53_out1.sqrt();
        let constant449_out1 = self.constant449.val();
        let div28_out1 = constant449_out1.div(sqrt16_out1);
        let transpose20_out1 = add37_out1.permute([0, 1, 3, 2]);
        let sqrt17_out1 = div28_out1.sqrt();
        let mul67_out1 = add36_out1
            .mul((sqrt17_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul68_out1 = transpose20_out1
            .mul((sqrt17_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul34_out1 = mul67_out1.matmul(mul68_out1);
        let add38_out1 = matmul34_out1.add(where3_out1);
        let softmax6_out1 = burn::tensor::activation::softmax(add38_out1, 3);
        let matmul35_out1 = softmax6_out1.matmul(squeeze18_out1);
        let transpose21_out1 = matmul35_out1.permute([0, 2, 1, 3]);
        let unsqueeze52_out1 = [gather27_out1 as i64];
        let constant452_out1: [i64; 1] = [768i64];
        let constant451_out1: [i64; 1] = [-1i64];
        let concat29_out1: [i64; 3usize] = [
            &unsqueeze52_out1[..],
            &constant451_out1[..],
            &constant452_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape15_out1 = transpose21_out1.reshape(concat29_out1);
        let linear22_out1 = self.linear22.forward(reshape15_out1);
        let add39_out1 = add35_out1.add(linear22_out1);
        add39_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule7 {
    layernormalization12: LayerNorm,
    linear23: Linear,
    constant460: burn::module::Param<Tensor<1>>,
    constant461: burn::module::Param<Tensor<1>>,
    constant462: burn::module::Param<Tensor<1>>,
    linear24: Linear,
    layernormalization13: LayerNorm,
    linear25: Linear,
    constant496: burn::module::Param<Tensor<1>>,
    linear26: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule7 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization12 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear23 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant460: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant461: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant462: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear24 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization13 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear25 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant496: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear26 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization12,
            linear23,
            constant460,
            constant461,
            constant462,
            linear24,
            layernormalization13,
            linear25,
            constant496,
            linear26,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add39_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization12_out1 = {
            let dtype = add39_out1.clone().dtype();
            self.layernormalization12
                .forward(add39_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear23_out1 = self.linear23.forward(layernormalization12_out1);
        let shape38_out1: [i64; 3] = {
            let axes = &linear23_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant454_out1: [i64; 1] = [-1i64];
        let gather30_out1: [i64; 1usize] = constant454_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape38_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape38_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant456_out1: [i64; 1] = [1i64];
        let add40_out1 = {
            let __lhs = gather30_out1;
            let __rhs = constant456_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant457_out1: [i64; 1] = [2i64];
        let div29_out1 = {
            let __lhs = add40_out1;
            let __rhs = constant457_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice41_out1 = linear23_out1.clone().slice(s![.., .., 0..div29_out1[0]]);
        let constant459_out1: [i64; 1] = [2i64];
        let mul70_out1 = {
            let __lhs = div29_out1;
            let __rhs = constant459_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice42_out1 = linear23_out1.slice(s![.., .., div29_out1[0]..mul70_out1[0]]);
        let constant460_out1 = self.constant460.val();
        let div30_out1 = slice41_out1
            .clone()
            .div((constant460_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf6_out1 = div30_out1.erf();
        let constant461_out1 = self.constant461.val();
        let add41_out1 = erf6_out1
            .add((constant461_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul71_out1 = slice41_out1.mul(add41_out1);
        let constant462_out1 = self.constant462.val();
        let mul72_out1 = mul71_out1
            .mul((constant462_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul73_out1 = mul72_out1.mul(slice42_out1);
        let linear24_out1 = self.linear24.forward(mul73_out1);
        let add42_out1 = add39_out1.add(linear24_out1);
        let layernormalization13_out1 = {
            let dtype = add42_out1.clone().dtype();
            self.layernormalization13
                .forward(add42_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear25_out1 = self.linear25.forward(layernormalization13_out1.clone());
        let shape39_out1: [i64; 3] = {
            let axes = &layernormalization13_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather31_out1 = shape39_out1[0] as i64;
        let unsqueeze53_out1 = [gather31_out1 as i64];
        let constant469_out1: [i64; 1] = [64i64];
        let constant466_out1: [i64; 1] = [-1i64];
        let constant467_out1: [i64; 1] = [3i64];
        let constant468_out1: [i64; 1] = [12i64];
        let concat30_out1: [i64; 5usize] = [
            &unsqueeze53_out1[..],
            &constant466_out1[..],
            &constant467_out1[..],
            &constant468_out1[..],
            &constant469_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape16_out1 = linear25_out1.reshape(concat30_out1);
        let transpose22_out1 = reshape16_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose22_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split7_out1, split7_out2, split7_out3] = split_tensors.try_into().unwrap();
        let squeeze19_out1 = split7_out1.squeeze_dims::<4>(&[2]);
        let squeeze20_out1 = split7_out2.squeeze_dims::<4>(&[2]);
        let squeeze21_out1 = split7_out3.squeeze_dims::<4>(&[2]);
        let mul74_out1 = squeeze19_out1.clone().mul(unsqueeze13_out1.clone());
        let shape40_out1: [i64; 4] = {
            let axes = &squeeze19_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather32_out1 = shape40_out1[3] as i64;
        let constant475_out1 = 2i64;
        let div31_out1 = gather32_out1 / constant475_out1;
        let unsqueeze54_out1 = [div31_out1 as i64];
        let slice43_out1 = squeeze19_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze54_out1[0]]);
        let unsqueeze55_out1 = [div31_out1 as i64];
        let slice44_out1 = squeeze19_out1
            .slice(s![.., .., .., unsqueeze55_out1[0]..9223372036854775807]);
        let neg13_out1 = slice44_out1.neg();
        let concat31_out1 = burn::tensor::Tensor::cat(
            [neg13_out1, slice43_out1].into(),
            3,
        );
        let mul75_out1 = concat31_out1.mul(unsqueeze14_out1.clone());
        let add43_out1 = mul74_out1.add(mul75_out1);
        let mul76_out1 = squeeze20_out1.clone().mul(unsqueeze13_out1);
        let shape41_out1: [i64; 4] = {
            let axes = &squeeze20_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather33_out1 = shape41_out1[3] as i64;
        let constant485_out1 = 2i64;
        let div32_out1 = gather33_out1 / constant485_out1;
        let unsqueeze56_out1 = [div32_out1 as i64];
        let slice45_out1 = squeeze20_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze56_out1[0]]);
        let unsqueeze57_out1 = [div32_out1 as i64];
        let slice46_out1 = squeeze20_out1
            .slice(s![.., .., .., unsqueeze57_out1[0]..9223372036854775807]);
        let neg14_out1 = slice46_out1.neg();
        let concat32_out1 = burn::tensor::Tensor::cat(
            [neg14_out1, slice45_out1].into(),
            3,
        );
        let mul77_out1 = concat32_out1.mul(unsqueeze14_out1);
        let add44_out1 = mul76_out1.add(mul77_out1);
        let shape42_out1: [i64; 4] = {
            let axes = &add43_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice47_out1: [i64; 1] = shape42_out1[3..4].try_into().unwrap();
        let cast59_out1 = {
            let shape_array = slice47_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt19_out1 = cast59_out1.sqrt();
        let constant496_out1 = self.constant496.val();
        let div33_out1 = constant496_out1.div(sqrt19_out1);
        let transpose23_out1 = add44_out1.permute([0, 1, 3, 2]);
        let sqrt20_out1 = div33_out1.sqrt();
        let mul78_out1 = add43_out1
            .mul((sqrt20_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul79_out1 = transpose23_out1
            .mul((sqrt20_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul40_out1 = mul78_out1.matmul(mul79_out1);
        let add45_out1 = matmul40_out1.add(where2_out1);
        let softmax7_out1 = burn::tensor::activation::softmax(add45_out1, 3);
        let matmul41_out1 = softmax7_out1.matmul(squeeze21_out1);
        let transpose24_out1 = matmul41_out1.permute([0, 2, 1, 3]);
        let unsqueeze58_out1 = [gather31_out1 as i64];
        let constant499_out1: [i64; 1] = [768i64];
        let constant498_out1: [i64; 1] = [-1i64];
        let concat33_out1: [i64; 3usize] = [
            &unsqueeze58_out1[..],
            &constant498_out1[..],
            &constant499_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape17_out1 = transpose24_out1.reshape(concat33_out1);
        let linear26_out1 = self.linear26.forward(reshape17_out1);
        let add46_out1 = add42_out1.add(linear26_out1);
        add46_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule8 {
    layernormalization14: LayerNorm,
    linear27: Linear,
    constant507: burn::module::Param<Tensor<1>>,
    constant508: burn::module::Param<Tensor<1>>,
    constant509: burn::module::Param<Tensor<1>>,
    linear28: Linear,
    layernormalization15: LayerNorm,
    linear29: Linear,
    constant543: burn::module::Param<Tensor<1>>,
    linear30: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule8 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization14 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear27 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant507: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant508: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant509: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear28 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization15 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear29 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant543: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear30 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization14,
            linear27,
            constant507,
            constant508,
            constant509,
            linear28,
            layernormalization15,
            linear29,
            constant543,
            linear30,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add46_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization14_out1 = {
            let dtype = add46_out1.clone().dtype();
            self.layernormalization14
                .forward(add46_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear27_out1 = self.linear27.forward(layernormalization14_out1);
        let shape43_out1: [i64; 3] = {
            let axes = &linear27_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant501_out1: [i64; 1] = [-1i64];
        let gather34_out1: [i64; 1usize] = constant501_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape43_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape43_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant503_out1: [i64; 1] = [1i64];
        let add47_out1 = {
            let __lhs = gather34_out1;
            let __rhs = constant503_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant504_out1: [i64; 1] = [2i64];
        let div34_out1 = {
            let __lhs = add47_out1;
            let __rhs = constant504_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice48_out1 = linear27_out1.clone().slice(s![.., .., 0..div34_out1[0]]);
        let constant506_out1: [i64; 1] = [2i64];
        let mul81_out1 = {
            let __lhs = div34_out1;
            let __rhs = constant506_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice49_out1 = linear27_out1.slice(s![.., .., div34_out1[0]..mul81_out1[0]]);
        let constant507_out1 = self.constant507.val();
        let div35_out1 = slice48_out1
            .clone()
            .div((constant507_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf7_out1 = div35_out1.erf();
        let constant508_out1 = self.constant508.val();
        let add48_out1 = erf7_out1
            .add((constant508_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul82_out1 = slice48_out1.mul(add48_out1);
        let constant509_out1 = self.constant509.val();
        let mul83_out1 = mul82_out1
            .mul((constant509_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul84_out1 = mul83_out1.mul(slice49_out1);
        let linear28_out1 = self.linear28.forward(mul84_out1);
        let add49_out1 = add46_out1.add(linear28_out1);
        let layernormalization15_out1 = {
            let dtype = add49_out1.clone().dtype();
            self.layernormalization15
                .forward(add49_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear29_out1 = self.linear29.forward(layernormalization15_out1.clone());
        let shape44_out1: [i64; 3] = {
            let axes = &layernormalization15_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather35_out1 = shape44_out1[0] as i64;
        let unsqueeze59_out1 = [gather35_out1 as i64];
        let constant516_out1: [i64; 1] = [64i64];
        let constant513_out1: [i64; 1] = [-1i64];
        let constant514_out1: [i64; 1] = [3i64];
        let constant515_out1: [i64; 1] = [12i64];
        let concat34_out1: [i64; 5usize] = [
            &unsqueeze59_out1[..],
            &constant513_out1[..],
            &constant514_out1[..],
            &constant515_out1[..],
            &constant516_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape18_out1 = linear29_out1.reshape(concat34_out1);
        let transpose25_out1 = reshape18_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose25_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split8_out1, split8_out2, split8_out3] = split_tensors.try_into().unwrap();
        let squeeze22_out1 = split8_out1.squeeze_dims::<4>(&[2]);
        let squeeze23_out1 = split8_out2.squeeze_dims::<4>(&[2]);
        let squeeze24_out1 = split8_out3.squeeze_dims::<4>(&[2]);
        let mul85_out1 = squeeze22_out1.clone().mul(unsqueeze22_out1.clone());
        let shape45_out1: [i64; 4] = {
            let axes = &squeeze22_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather36_out1 = shape45_out1[3] as i64;
        let constant522_out1 = 2i64;
        let div36_out1 = gather36_out1 / constant522_out1;
        let unsqueeze60_out1 = [div36_out1 as i64];
        let slice50_out1 = squeeze22_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze60_out1[0]]);
        let unsqueeze61_out1 = [div36_out1 as i64];
        let slice51_out1 = squeeze22_out1
            .slice(s![.., .., .., unsqueeze61_out1[0]..9223372036854775807]);
        let neg15_out1 = slice51_out1.neg();
        let concat35_out1 = burn::tensor::Tensor::cat(
            [neg15_out1, slice50_out1].into(),
            3,
        );
        let mul86_out1 = concat35_out1.mul(unsqueeze23_out1.clone());
        let add50_out1 = mul85_out1.add(mul86_out1);
        let mul87_out1 = squeeze23_out1.clone().mul(unsqueeze22_out1);
        let shape46_out1: [i64; 4] = {
            let axes = &squeeze23_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather37_out1 = shape46_out1[3] as i64;
        let constant532_out1 = 2i64;
        let div37_out1 = gather37_out1 / constant532_out1;
        let unsqueeze62_out1 = [div37_out1 as i64];
        let slice52_out1 = squeeze23_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze62_out1[0]]);
        let unsqueeze63_out1 = [div37_out1 as i64];
        let slice53_out1 = squeeze23_out1
            .slice(s![.., .., .., unsqueeze63_out1[0]..9223372036854775807]);
        let neg16_out1 = slice53_out1.neg();
        let concat36_out1 = burn::tensor::Tensor::cat(
            [neg16_out1, slice52_out1].into(),
            3,
        );
        let mul88_out1 = concat36_out1.mul(unsqueeze23_out1);
        let add51_out1 = mul87_out1.add(mul88_out1);
        let shape47_out1: [i64; 4] = {
            let axes = &add50_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice54_out1: [i64; 1] = shape47_out1[3..4].try_into().unwrap();
        let cast65_out1 = {
            let shape_array = slice54_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt22_out1 = cast65_out1.sqrt();
        let constant543_out1 = self.constant543.val();
        let div38_out1 = constant543_out1.div(sqrt22_out1);
        let transpose26_out1 = add51_out1.permute([0, 1, 3, 2]);
        let sqrt23_out1 = div38_out1.sqrt();
        let mul89_out1 = add50_out1
            .mul((sqrt23_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul90_out1 = transpose26_out1
            .mul((sqrt23_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul46_out1 = mul89_out1.matmul(mul90_out1);
        let add52_out1 = matmul46_out1.add(where3_out1);
        let softmax8_out1 = burn::tensor::activation::softmax(add52_out1, 3);
        let matmul47_out1 = softmax8_out1.matmul(squeeze24_out1);
        let transpose27_out1 = matmul47_out1.permute([0, 2, 1, 3]);
        let unsqueeze64_out1 = [gather35_out1 as i64];
        let constant546_out1: [i64; 1] = [768i64];
        let constant545_out1: [i64; 1] = [-1i64];
        let concat37_out1: [i64; 3usize] = [
            &unsqueeze64_out1[..],
            &constant545_out1[..],
            &constant546_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape19_out1 = transpose27_out1.reshape(concat37_out1);
        let linear30_out1 = self.linear30.forward(reshape19_out1);
        let add53_out1 = add49_out1.add(linear30_out1);
        add53_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule9 {
    layernormalization16: LayerNorm,
    linear31: Linear,
    constant554: burn::module::Param<Tensor<1>>,
    constant555: burn::module::Param<Tensor<1>>,
    constant556: burn::module::Param<Tensor<1>>,
    linear32: Linear,
    layernormalization17: LayerNorm,
    linear33: Linear,
    constant590: burn::module::Param<Tensor<1>>,
    linear34: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule9 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization16 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear31 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant554: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant555: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant556: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear32 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization17 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear33 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant590: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear34 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization16,
            linear31,
            constant554,
            constant555,
            constant556,
            linear32,
            layernormalization17,
            linear33,
            constant590,
            linear34,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add53_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization16_out1 = {
            let dtype = add53_out1.clone().dtype();
            self.layernormalization16
                .forward(add53_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear31_out1 = self.linear31.forward(layernormalization16_out1);
        let shape48_out1: [i64; 3] = {
            let axes = &linear31_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant548_out1: [i64; 1] = [-1i64];
        let gather38_out1: [i64; 1usize] = constant548_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape48_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape48_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant550_out1: [i64; 1] = [1i64];
        let add54_out1 = {
            let __lhs = gather38_out1;
            let __rhs = constant550_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant551_out1: [i64; 1] = [2i64];
        let div39_out1 = {
            let __lhs = add54_out1;
            let __rhs = constant551_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice55_out1 = linear31_out1.clone().slice(s![.., .., 0..div39_out1[0]]);
        let constant553_out1: [i64; 1] = [2i64];
        let mul92_out1 = {
            let __lhs = div39_out1;
            let __rhs = constant553_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice56_out1 = linear31_out1.slice(s![.., .., div39_out1[0]..mul92_out1[0]]);
        let constant554_out1 = self.constant554.val();
        let div40_out1 = slice55_out1
            .clone()
            .div((constant554_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf8_out1 = div40_out1.erf();
        let constant555_out1 = self.constant555.val();
        let add55_out1 = erf8_out1
            .add((constant555_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul93_out1 = slice55_out1.mul(add55_out1);
        let constant556_out1 = self.constant556.val();
        let mul94_out1 = mul93_out1
            .mul((constant556_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul95_out1 = mul94_out1.mul(slice56_out1);
        let linear32_out1 = self.linear32.forward(mul95_out1);
        let add56_out1 = add53_out1.add(linear32_out1);
        let layernormalization17_out1 = {
            let dtype = add56_out1.clone().dtype();
            self.layernormalization17
                .forward(add56_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear33_out1 = self.linear33.forward(layernormalization17_out1.clone());
        let shape49_out1: [i64; 3] = {
            let axes = &layernormalization17_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather39_out1 = shape49_out1[0] as i64;
        let unsqueeze65_out1 = [gather39_out1 as i64];
        let constant563_out1: [i64; 1] = [64i64];
        let constant560_out1: [i64; 1] = [-1i64];
        let constant561_out1: [i64; 1] = [3i64];
        let constant562_out1: [i64; 1] = [12i64];
        let concat38_out1: [i64; 5usize] = [
            &unsqueeze65_out1[..],
            &constant560_out1[..],
            &constant561_out1[..],
            &constant562_out1[..],
            &constant563_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape20_out1 = linear33_out1.reshape(concat38_out1);
        let transpose28_out1 = reshape20_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose28_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split9_out1, split9_out2, split9_out3] = split_tensors.try_into().unwrap();
        let squeeze25_out1 = split9_out1.squeeze_dims::<4>(&[2]);
        let squeeze26_out1 = split9_out2.squeeze_dims::<4>(&[2]);
        let squeeze27_out1 = split9_out3.squeeze_dims::<4>(&[2]);
        let mul96_out1 = squeeze25_out1.clone().mul(unsqueeze22_out1.clone());
        let shape50_out1: [i64; 4] = {
            let axes = &squeeze25_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather40_out1 = shape50_out1[3] as i64;
        let constant569_out1 = 2i64;
        let div41_out1 = gather40_out1 / constant569_out1;
        let unsqueeze66_out1 = [div41_out1 as i64];
        let slice57_out1 = squeeze25_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze66_out1[0]]);
        let unsqueeze67_out1 = [div41_out1 as i64];
        let slice58_out1 = squeeze25_out1
            .slice(s![.., .., .., unsqueeze67_out1[0]..9223372036854775807]);
        let neg17_out1 = slice58_out1.neg();
        let concat39_out1 = burn::tensor::Tensor::cat(
            [neg17_out1, slice57_out1].into(),
            3,
        );
        let mul97_out1 = concat39_out1.mul(unsqueeze23_out1.clone());
        let add57_out1 = mul96_out1.add(mul97_out1);
        let mul98_out1 = squeeze26_out1.clone().mul(unsqueeze22_out1);
        let shape51_out1: [i64; 4] = {
            let axes = &squeeze26_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather41_out1 = shape51_out1[3] as i64;
        let constant579_out1 = 2i64;
        let div42_out1 = gather41_out1 / constant579_out1;
        let unsqueeze68_out1 = [div42_out1 as i64];
        let slice59_out1 = squeeze26_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze68_out1[0]]);
        let unsqueeze69_out1 = [div42_out1 as i64];
        let slice60_out1 = squeeze26_out1
            .slice(s![.., .., .., unsqueeze69_out1[0]..9223372036854775807]);
        let neg18_out1 = slice60_out1.neg();
        let concat40_out1 = burn::tensor::Tensor::cat(
            [neg18_out1, slice59_out1].into(),
            3,
        );
        let mul99_out1 = concat40_out1.mul(unsqueeze23_out1);
        let add58_out1 = mul98_out1.add(mul99_out1);
        let shape52_out1: [i64; 4] = {
            let axes = &add57_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice61_out1: [i64; 1] = shape52_out1[3..4].try_into().unwrap();
        let cast71_out1 = {
            let shape_array = slice61_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt25_out1 = cast71_out1.sqrt();
        let constant590_out1 = self.constant590.val();
        let div43_out1 = constant590_out1.div(sqrt25_out1);
        let transpose29_out1 = add58_out1.permute([0, 1, 3, 2]);
        let sqrt26_out1 = div43_out1.sqrt();
        let mul100_out1 = add57_out1
            .mul((sqrt26_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul101_out1 = transpose29_out1
            .mul((sqrt26_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul52_out1 = mul100_out1.matmul(mul101_out1);
        let add59_out1 = matmul52_out1.add(where3_out1);
        let softmax9_out1 = burn::tensor::activation::softmax(add59_out1, 3);
        let matmul53_out1 = softmax9_out1.matmul(squeeze27_out1);
        let transpose30_out1 = matmul53_out1.permute([0, 2, 1, 3]);
        let unsqueeze70_out1 = [gather39_out1 as i64];
        let constant593_out1: [i64; 1] = [768i64];
        let constant592_out1: [i64; 1] = [-1i64];
        let concat41_out1: [i64; 3usize] = [
            &unsqueeze70_out1[..],
            &constant592_out1[..],
            &constant593_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape21_out1 = transpose30_out1.reshape(concat41_out1);
        let linear34_out1 = self.linear34.forward(reshape21_out1);
        let add60_out1 = add56_out1.add(linear34_out1);
        add60_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule10 {
    layernormalization18: LayerNorm,
    linear35: Linear,
    constant601: burn::module::Param<Tensor<1>>,
    constant602: burn::module::Param<Tensor<1>>,
    constant603: burn::module::Param<Tensor<1>>,
    linear36: Linear,
    layernormalization19: LayerNorm,
    linear37: Linear,
    constant637: burn::module::Param<Tensor<1>>,
    linear38: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule10 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization18 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear35 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant601: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant602: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant603: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear36 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization19 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear37 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant637: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear38 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization18,
            linear35,
            constant601,
            constant602,
            constant603,
            linear36,
            layernormalization19,
            linear37,
            constant637,
            linear38,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add60_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization18_out1 = {
            let dtype = add60_out1.clone().dtype();
            self.layernormalization18
                .forward(add60_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear35_out1 = self.linear35.forward(layernormalization18_out1);
        let shape53_out1: [i64; 3] = {
            let axes = &linear35_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant595_out1: [i64; 1] = [-1i64];
        let gather42_out1: [i64; 1usize] = constant595_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape53_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape53_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant597_out1: [i64; 1] = [1i64];
        let add61_out1 = {
            let __lhs = gather42_out1;
            let __rhs = constant597_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant598_out1: [i64; 1] = [2i64];
        let div44_out1 = {
            let __lhs = add61_out1;
            let __rhs = constant598_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice62_out1 = linear35_out1.clone().slice(s![.., .., 0..div44_out1[0]]);
        let constant600_out1: [i64; 1] = [2i64];
        let mul103_out1 = {
            let __lhs = div44_out1;
            let __rhs = constant600_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice63_out1 = linear35_out1
            .slice(s![.., .., div44_out1[0]..mul103_out1[0]]);
        let constant601_out1 = self.constant601.val();
        let div45_out1 = slice62_out1
            .clone()
            .div((constant601_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf9_out1 = div45_out1.erf();
        let constant602_out1 = self.constant602.val();
        let add62_out1 = erf9_out1
            .add((constant602_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul104_out1 = slice62_out1.mul(add62_out1);
        let constant603_out1 = self.constant603.val();
        let mul105_out1 = mul104_out1
            .mul((constant603_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul106_out1 = mul105_out1.mul(slice63_out1);
        let linear36_out1 = self.linear36.forward(mul106_out1);
        let add63_out1 = add60_out1.add(linear36_out1);
        let layernormalization19_out1 = {
            let dtype = add63_out1.clone().dtype();
            self.layernormalization19
                .forward(add63_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear37_out1 = self.linear37.forward(layernormalization19_out1.clone());
        let shape54_out1: [i64; 3] = {
            let axes = &layernormalization19_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather43_out1 = shape54_out1[0] as i64;
        let unsqueeze71_out1 = [gather43_out1 as i64];
        let constant610_out1: [i64; 1] = [64i64];
        let constant607_out1: [i64; 1] = [-1i64];
        let constant608_out1: [i64; 1] = [3i64];
        let constant609_out1: [i64; 1] = [12i64];
        let concat42_out1: [i64; 5usize] = [
            &unsqueeze71_out1[..],
            &constant607_out1[..],
            &constant608_out1[..],
            &constant609_out1[..],
            &constant610_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape22_out1 = linear37_out1.reshape(concat42_out1);
        let transpose31_out1 = reshape22_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose31_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split10_out1, split10_out2, split10_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze28_out1 = split10_out1.squeeze_dims::<4>(&[2]);
        let squeeze29_out1 = split10_out2.squeeze_dims::<4>(&[2]);
        let squeeze30_out1 = split10_out3.squeeze_dims::<4>(&[2]);
        let mul107_out1 = squeeze28_out1.clone().mul(unsqueeze13_out1.clone());
        let shape55_out1: [i64; 4] = {
            let axes = &squeeze28_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather44_out1 = shape55_out1[3] as i64;
        let constant616_out1 = 2i64;
        let div46_out1 = gather44_out1 / constant616_out1;
        let unsqueeze72_out1 = [div46_out1 as i64];
        let slice64_out1 = squeeze28_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze72_out1[0]]);
        let unsqueeze73_out1 = [div46_out1 as i64];
        let slice65_out1 = squeeze28_out1
            .slice(s![.., .., .., unsqueeze73_out1[0]..9223372036854775807]);
        let neg19_out1 = slice65_out1.neg();
        let concat43_out1 = burn::tensor::Tensor::cat(
            [neg19_out1, slice64_out1].into(),
            3,
        );
        let mul108_out1 = concat43_out1.mul(unsqueeze14_out1.clone());
        let add64_out1 = mul107_out1.add(mul108_out1);
        let mul109_out1 = squeeze29_out1.clone().mul(unsqueeze13_out1);
        let shape56_out1: [i64; 4] = {
            let axes = &squeeze29_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather45_out1 = shape56_out1[3] as i64;
        let constant626_out1 = 2i64;
        let div47_out1 = gather45_out1 / constant626_out1;
        let unsqueeze74_out1 = [div47_out1 as i64];
        let slice66_out1 = squeeze29_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze74_out1[0]]);
        let unsqueeze75_out1 = [div47_out1 as i64];
        let slice67_out1 = squeeze29_out1
            .slice(s![.., .., .., unsqueeze75_out1[0]..9223372036854775807]);
        let neg20_out1 = slice67_out1.neg();
        let concat44_out1 = burn::tensor::Tensor::cat(
            [neg20_out1, slice66_out1].into(),
            3,
        );
        let mul110_out1 = concat44_out1.mul(unsqueeze14_out1);
        let add65_out1 = mul109_out1.add(mul110_out1);
        let shape57_out1: [i64; 4] = {
            let axes = &add64_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice68_out1: [i64; 1] = shape57_out1[3..4].try_into().unwrap();
        let cast77_out1 = {
            let shape_array = slice68_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt28_out1 = cast77_out1.sqrt();
        let constant637_out1 = self.constant637.val();
        let div48_out1 = constant637_out1.div(sqrt28_out1);
        let transpose32_out1 = add65_out1.permute([0, 1, 3, 2]);
        let sqrt29_out1 = div48_out1.sqrt();
        let mul111_out1 = add64_out1
            .mul((sqrt29_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul112_out1 = transpose32_out1
            .mul((sqrt29_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul58_out1 = mul111_out1.matmul(mul112_out1);
        let add66_out1 = matmul58_out1.add(where2_out1);
        let softmax10_out1 = burn::tensor::activation::softmax(add66_out1, 3);
        let matmul59_out1 = softmax10_out1.matmul(squeeze30_out1);
        let transpose33_out1 = matmul59_out1.permute([0, 2, 1, 3]);
        let unsqueeze76_out1 = [gather43_out1 as i64];
        let constant640_out1: [i64; 1] = [768i64];
        let constant639_out1: [i64; 1] = [-1i64];
        let concat45_out1: [i64; 3usize] = [
            &unsqueeze76_out1[..],
            &constant639_out1[..],
            &constant640_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape23_out1 = transpose33_out1.reshape(concat45_out1);
        let linear38_out1 = self.linear38.forward(reshape23_out1);
        let add67_out1 = add63_out1.add(linear38_out1);
        add67_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule11 {
    layernormalization20: LayerNorm,
    linear39: Linear,
    constant648: burn::module::Param<Tensor<1>>,
    constant649: burn::module::Param<Tensor<1>>,
    constant650: burn::module::Param<Tensor<1>>,
    linear40: Linear,
    layernormalization21: LayerNorm,
    linear41: Linear,
    constant684: burn::module::Param<Tensor<1>>,
    linear42: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule11 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization20 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear39 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant648: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant649: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant650: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear40 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization21 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear41 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant684: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear42 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization20,
            linear39,
            constant648,
            constant649,
            constant650,
            linear40,
            layernormalization21,
            linear41,
            constant684,
            linear42,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add67_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization20_out1 = {
            let dtype = add67_out1.clone().dtype();
            self.layernormalization20
                .forward(add67_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear39_out1 = self.linear39.forward(layernormalization20_out1);
        let shape58_out1: [i64; 3] = {
            let axes = &linear39_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant642_out1: [i64; 1] = [-1i64];
        let gather46_out1: [i64; 1usize] = constant642_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape58_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape58_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant644_out1: [i64; 1] = [1i64];
        let add68_out1 = {
            let __lhs = gather46_out1;
            let __rhs = constant644_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant645_out1: [i64; 1] = [2i64];
        let div49_out1 = {
            let __lhs = add68_out1;
            let __rhs = constant645_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice69_out1 = linear39_out1.clone().slice(s![.., .., 0..div49_out1[0]]);
        let constant647_out1: [i64; 1] = [2i64];
        let mul114_out1 = {
            let __lhs = div49_out1;
            let __rhs = constant647_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice70_out1 = linear39_out1
            .slice(s![.., .., div49_out1[0]..mul114_out1[0]]);
        let constant648_out1 = self.constant648.val();
        let div50_out1 = slice69_out1
            .clone()
            .div((constant648_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf10_out1 = div50_out1.erf();
        let constant649_out1 = self.constant649.val();
        let add69_out1 = erf10_out1
            .add((constant649_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul115_out1 = slice69_out1.mul(add69_out1);
        let constant650_out1 = self.constant650.val();
        let mul116_out1 = mul115_out1
            .mul((constant650_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul117_out1 = mul116_out1.mul(slice70_out1);
        let linear40_out1 = self.linear40.forward(mul117_out1);
        let add70_out1 = add67_out1.add(linear40_out1);
        let layernormalization21_out1 = {
            let dtype = add70_out1.clone().dtype();
            self.layernormalization21
                .forward(add70_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear41_out1 = self.linear41.forward(layernormalization21_out1.clone());
        let shape59_out1: [i64; 3] = {
            let axes = &layernormalization21_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather47_out1 = shape59_out1[0] as i64;
        let unsqueeze77_out1 = [gather47_out1 as i64];
        let constant657_out1: [i64; 1] = [64i64];
        let constant654_out1: [i64; 1] = [-1i64];
        let constant655_out1: [i64; 1] = [3i64];
        let constant656_out1: [i64; 1] = [12i64];
        let concat46_out1: [i64; 5usize] = [
            &unsqueeze77_out1[..],
            &constant654_out1[..],
            &constant655_out1[..],
            &constant656_out1[..],
            &constant657_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape24_out1 = linear41_out1.reshape(concat46_out1);
        let transpose34_out1 = reshape24_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose34_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split11_out1, split11_out2, split11_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze31_out1 = split11_out1.squeeze_dims::<4>(&[2]);
        let squeeze32_out1 = split11_out2.squeeze_dims::<4>(&[2]);
        let squeeze33_out1 = split11_out3.squeeze_dims::<4>(&[2]);
        let mul118_out1 = squeeze31_out1.clone().mul(unsqueeze22_out1.clone());
        let shape60_out1: [i64; 4] = {
            let axes = &squeeze31_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather48_out1 = shape60_out1[3] as i64;
        let constant663_out1 = 2i64;
        let div51_out1 = gather48_out1 / constant663_out1;
        let unsqueeze78_out1 = [div51_out1 as i64];
        let slice71_out1 = squeeze31_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze78_out1[0]]);
        let unsqueeze79_out1 = [div51_out1 as i64];
        let slice72_out1 = squeeze31_out1
            .slice(s![.., .., .., unsqueeze79_out1[0]..9223372036854775807]);
        let neg21_out1 = slice72_out1.neg();
        let concat47_out1 = burn::tensor::Tensor::cat(
            [neg21_out1, slice71_out1].into(),
            3,
        );
        let mul119_out1 = concat47_out1.mul(unsqueeze23_out1.clone());
        let add71_out1 = mul118_out1.add(mul119_out1);
        let mul120_out1 = squeeze32_out1.clone().mul(unsqueeze22_out1);
        let shape61_out1: [i64; 4] = {
            let axes = &squeeze32_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather49_out1 = shape61_out1[3] as i64;
        let constant673_out1 = 2i64;
        let div52_out1 = gather49_out1 / constant673_out1;
        let unsqueeze80_out1 = [div52_out1 as i64];
        let slice73_out1 = squeeze32_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze80_out1[0]]);
        let unsqueeze81_out1 = [div52_out1 as i64];
        let slice74_out1 = squeeze32_out1
            .slice(s![.., .., .., unsqueeze81_out1[0]..9223372036854775807]);
        let neg22_out1 = slice74_out1.neg();
        let concat48_out1 = burn::tensor::Tensor::cat(
            [neg22_out1, slice73_out1].into(),
            3,
        );
        let mul121_out1 = concat48_out1.mul(unsqueeze23_out1);
        let add72_out1 = mul120_out1.add(mul121_out1);
        let shape62_out1: [i64; 4] = {
            let axes = &add71_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice75_out1: [i64; 1] = shape62_out1[3..4].try_into().unwrap();
        let cast83_out1 = {
            let shape_array = slice75_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt31_out1 = cast83_out1.sqrt();
        let constant684_out1 = self.constant684.val();
        let div53_out1 = constant684_out1.div(sqrt31_out1);
        let transpose35_out1 = add72_out1.permute([0, 1, 3, 2]);
        let sqrt32_out1 = div53_out1.sqrt();
        let mul122_out1 = add71_out1
            .mul((sqrt32_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul123_out1 = transpose35_out1
            .mul((sqrt32_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul64_out1 = mul122_out1.matmul(mul123_out1);
        let add73_out1 = matmul64_out1.add(where3_out1);
        let softmax11_out1 = burn::tensor::activation::softmax(add73_out1, 3);
        let matmul65_out1 = softmax11_out1.matmul(squeeze33_out1);
        let transpose36_out1 = matmul65_out1.permute([0, 2, 1, 3]);
        let unsqueeze82_out1 = [gather47_out1 as i64];
        let constant687_out1: [i64; 1] = [768i64];
        let constant686_out1: [i64; 1] = [-1i64];
        let concat49_out1: [i64; 3usize] = [
            &unsqueeze82_out1[..],
            &constant686_out1[..],
            &constant687_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape25_out1 = transpose36_out1.reshape(concat49_out1);
        let linear42_out1 = self.linear42.forward(reshape25_out1);
        let add74_out1 = add70_out1.add(linear42_out1);
        add74_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule12 {
    layernormalization22: LayerNorm,
    linear43: Linear,
    constant695: burn::module::Param<Tensor<1>>,
    constant696: burn::module::Param<Tensor<1>>,
    constant697: burn::module::Param<Tensor<1>>,
    linear44: Linear,
    layernormalization23: LayerNorm,
    linear45: Linear,
    constant731: burn::module::Param<Tensor<1>>,
    linear46: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule12 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization22 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear43 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant695: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant696: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant697: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear44 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization23 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear45 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant731: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear46 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization22,
            linear43,
            constant695,
            constant696,
            constant697,
            linear44,
            layernormalization23,
            linear45,
            constant731,
            linear46,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add74_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization22_out1 = {
            let dtype = add74_out1.clone().dtype();
            self.layernormalization22
                .forward(add74_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear43_out1 = self.linear43.forward(layernormalization22_out1);
        let shape63_out1: [i64; 3] = {
            let axes = &linear43_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant689_out1: [i64; 1] = [-1i64];
        let gather50_out1: [i64; 1usize] = constant689_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape63_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape63_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant691_out1: [i64; 1] = [1i64];
        let add75_out1 = {
            let __lhs = gather50_out1;
            let __rhs = constant691_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant692_out1: [i64; 1] = [2i64];
        let div54_out1 = {
            let __lhs = add75_out1;
            let __rhs = constant692_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice76_out1 = linear43_out1.clone().slice(s![.., .., 0..div54_out1[0]]);
        let constant694_out1: [i64; 1] = [2i64];
        let mul125_out1 = {
            let __lhs = div54_out1;
            let __rhs = constant694_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice77_out1 = linear43_out1
            .slice(s![.., .., div54_out1[0]..mul125_out1[0]]);
        let constant695_out1 = self.constant695.val();
        let div55_out1 = slice76_out1
            .clone()
            .div((constant695_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf11_out1 = div55_out1.erf();
        let constant696_out1 = self.constant696.val();
        let add76_out1 = erf11_out1
            .add((constant696_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul126_out1 = slice76_out1.mul(add76_out1);
        let constant697_out1 = self.constant697.val();
        let mul127_out1 = mul126_out1
            .mul((constant697_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul128_out1 = mul127_out1.mul(slice77_out1);
        let linear44_out1 = self.linear44.forward(mul128_out1);
        let add77_out1 = add74_out1.add(linear44_out1);
        let layernormalization23_out1 = {
            let dtype = add77_out1.clone().dtype();
            self.layernormalization23
                .forward(add77_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear45_out1 = self.linear45.forward(layernormalization23_out1.clone());
        let shape64_out1: [i64; 3] = {
            let axes = &layernormalization23_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather51_out1 = shape64_out1[0] as i64;
        let unsqueeze83_out1 = [gather51_out1 as i64];
        let constant704_out1: [i64; 1] = [64i64];
        let constant701_out1: [i64; 1] = [-1i64];
        let constant702_out1: [i64; 1] = [3i64];
        let constant703_out1: [i64; 1] = [12i64];
        let concat50_out1: [i64; 5usize] = [
            &unsqueeze83_out1[..],
            &constant701_out1[..],
            &constant702_out1[..],
            &constant703_out1[..],
            &constant704_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape26_out1 = linear45_out1.reshape(concat50_out1);
        let transpose37_out1 = reshape26_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose37_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split12_out1, split12_out2, split12_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze34_out1 = split12_out1.squeeze_dims::<4>(&[2]);
        let squeeze35_out1 = split12_out2.squeeze_dims::<4>(&[2]);
        let squeeze36_out1 = split12_out3.squeeze_dims::<4>(&[2]);
        let mul129_out1 = squeeze34_out1.clone().mul(unsqueeze22_out1.clone());
        let shape65_out1: [i64; 4] = {
            let axes = &squeeze34_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather52_out1 = shape65_out1[3] as i64;
        let constant710_out1 = 2i64;
        let div56_out1 = gather52_out1 / constant710_out1;
        let unsqueeze84_out1 = [div56_out1 as i64];
        let slice78_out1 = squeeze34_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze84_out1[0]]);
        let unsqueeze85_out1 = [div56_out1 as i64];
        let slice79_out1 = squeeze34_out1
            .slice(s![.., .., .., unsqueeze85_out1[0]..9223372036854775807]);
        let neg23_out1 = slice79_out1.neg();
        let concat51_out1 = burn::tensor::Tensor::cat(
            [neg23_out1, slice78_out1].into(),
            3,
        );
        let mul130_out1 = concat51_out1.mul(unsqueeze23_out1.clone());
        let add78_out1 = mul129_out1.add(mul130_out1);
        let mul131_out1 = squeeze35_out1.clone().mul(unsqueeze22_out1);
        let shape66_out1: [i64; 4] = {
            let axes = &squeeze35_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather53_out1 = shape66_out1[3] as i64;
        let constant720_out1 = 2i64;
        let div57_out1 = gather53_out1 / constant720_out1;
        let unsqueeze86_out1 = [div57_out1 as i64];
        let slice80_out1 = squeeze35_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze86_out1[0]]);
        let unsqueeze87_out1 = [div57_out1 as i64];
        let slice81_out1 = squeeze35_out1
            .slice(s![.., .., .., unsqueeze87_out1[0]..9223372036854775807]);
        let neg24_out1 = slice81_out1.neg();
        let concat52_out1 = burn::tensor::Tensor::cat(
            [neg24_out1, slice80_out1].into(),
            3,
        );
        let mul132_out1 = concat52_out1.mul(unsqueeze23_out1);
        let add79_out1 = mul131_out1.add(mul132_out1);
        let shape67_out1: [i64; 4] = {
            let axes = &add78_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice82_out1: [i64; 1] = shape67_out1[3..4].try_into().unwrap();
        let cast89_out1 = {
            let shape_array = slice82_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt34_out1 = cast89_out1.sqrt();
        let constant731_out1 = self.constant731.val();
        let div58_out1 = constant731_out1.div(sqrt34_out1);
        let transpose38_out1 = add79_out1.permute([0, 1, 3, 2]);
        let sqrt35_out1 = div58_out1.sqrt();
        let mul133_out1 = add78_out1
            .mul((sqrt35_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul134_out1 = transpose38_out1
            .mul((sqrt35_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul70_out1 = mul133_out1.matmul(mul134_out1);
        let add80_out1 = matmul70_out1.add(where3_out1);
        let softmax12_out1 = burn::tensor::activation::softmax(add80_out1, 3);
        let matmul71_out1 = softmax12_out1.matmul(squeeze36_out1);
        let transpose39_out1 = matmul71_out1.permute([0, 2, 1, 3]);
        let unsqueeze88_out1 = [gather51_out1 as i64];
        let constant734_out1: [i64; 1] = [768i64];
        let constant733_out1: [i64; 1] = [-1i64];
        let concat53_out1: [i64; 3usize] = [
            &unsqueeze88_out1[..],
            &constant733_out1[..],
            &constant734_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape27_out1 = transpose39_out1.reshape(concat53_out1);
        let linear46_out1 = self.linear46.forward(reshape27_out1);
        let add81_out1 = add77_out1.add(linear46_out1);
        add81_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule13 {
    layernormalization24: LayerNorm,
    linear47: Linear,
    constant742: burn::module::Param<Tensor<1>>,
    constant743: burn::module::Param<Tensor<1>>,
    constant744: burn::module::Param<Tensor<1>>,
    linear48: Linear,
    layernormalization25: LayerNorm,
    linear49: Linear,
    constant778: burn::module::Param<Tensor<1>>,
    linear50: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule13 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization24 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear47 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant742: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant743: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant744: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear48 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization25 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear49 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant778: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear50 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization24,
            linear47,
            constant742,
            constant743,
            constant744,
            linear48,
            layernormalization25,
            linear49,
            constant778,
            linear50,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add81_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization24_out1 = {
            let dtype = add81_out1.clone().dtype();
            self.layernormalization24
                .forward(add81_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear47_out1 = self.linear47.forward(layernormalization24_out1);
        let shape68_out1: [i64; 3] = {
            let axes = &linear47_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant736_out1: [i64; 1] = [-1i64];
        let gather54_out1: [i64; 1usize] = constant736_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape68_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape68_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant738_out1: [i64; 1] = [1i64];
        let add82_out1 = {
            let __lhs = gather54_out1;
            let __rhs = constant738_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant739_out1: [i64; 1] = [2i64];
        let div59_out1 = {
            let __lhs = add82_out1;
            let __rhs = constant739_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice83_out1 = linear47_out1.clone().slice(s![.., .., 0..div59_out1[0]]);
        let constant741_out1: [i64; 1] = [2i64];
        let mul136_out1 = {
            let __lhs = div59_out1;
            let __rhs = constant741_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice84_out1 = linear47_out1
            .slice(s![.., .., div59_out1[0]..mul136_out1[0]]);
        let constant742_out1 = self.constant742.val();
        let div60_out1 = slice83_out1
            .clone()
            .div((constant742_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf12_out1 = div60_out1.erf();
        let constant743_out1 = self.constant743.val();
        let add83_out1 = erf12_out1
            .add((constant743_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul137_out1 = slice83_out1.mul(add83_out1);
        let constant744_out1 = self.constant744.val();
        let mul138_out1 = mul137_out1
            .mul((constant744_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul139_out1 = mul138_out1.mul(slice84_out1);
        let linear48_out1 = self.linear48.forward(mul139_out1);
        let add84_out1 = add81_out1.add(linear48_out1);
        let layernormalization25_out1 = {
            let dtype = add84_out1.clone().dtype();
            self.layernormalization25
                .forward(add84_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear49_out1 = self.linear49.forward(layernormalization25_out1.clone());
        let shape69_out1: [i64; 3] = {
            let axes = &layernormalization25_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather55_out1 = shape69_out1[0] as i64;
        let unsqueeze89_out1 = [gather55_out1 as i64];
        let constant751_out1: [i64; 1] = [64i64];
        let constant748_out1: [i64; 1] = [-1i64];
        let constant749_out1: [i64; 1] = [3i64];
        let constant750_out1: [i64; 1] = [12i64];
        let concat54_out1: [i64; 5usize] = [
            &unsqueeze89_out1[..],
            &constant748_out1[..],
            &constant749_out1[..],
            &constant750_out1[..],
            &constant751_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape28_out1 = linear49_out1.reshape(concat54_out1);
        let transpose40_out1 = reshape28_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose40_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split13_out1, split13_out2, split13_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze37_out1 = split13_out1.squeeze_dims::<4>(&[2]);
        let squeeze38_out1 = split13_out2.squeeze_dims::<4>(&[2]);
        let squeeze39_out1 = split13_out3.squeeze_dims::<4>(&[2]);
        let mul140_out1 = squeeze37_out1.clone().mul(unsqueeze13_out1.clone());
        let shape70_out1: [i64; 4] = {
            let axes = &squeeze37_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather56_out1 = shape70_out1[3] as i64;
        let constant757_out1 = 2i64;
        let div61_out1 = gather56_out1 / constant757_out1;
        let unsqueeze90_out1 = [div61_out1 as i64];
        let slice85_out1 = squeeze37_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze90_out1[0]]);
        let unsqueeze91_out1 = [div61_out1 as i64];
        let slice86_out1 = squeeze37_out1
            .slice(s![.., .., .., unsqueeze91_out1[0]..9223372036854775807]);
        let neg25_out1 = slice86_out1.neg();
        let concat55_out1 = burn::tensor::Tensor::cat(
            [neg25_out1, slice85_out1].into(),
            3,
        );
        let mul141_out1 = concat55_out1.mul(unsqueeze14_out1.clone());
        let add85_out1 = mul140_out1.add(mul141_out1);
        let mul142_out1 = squeeze38_out1.clone().mul(unsqueeze13_out1);
        let shape71_out1: [i64; 4] = {
            let axes = &squeeze38_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather57_out1 = shape71_out1[3] as i64;
        let constant767_out1 = 2i64;
        let div62_out1 = gather57_out1 / constant767_out1;
        let unsqueeze92_out1 = [div62_out1 as i64];
        let slice87_out1 = squeeze38_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze92_out1[0]]);
        let unsqueeze93_out1 = [div62_out1 as i64];
        let slice88_out1 = squeeze38_out1
            .slice(s![.., .., .., unsqueeze93_out1[0]..9223372036854775807]);
        let neg26_out1 = slice88_out1.neg();
        let concat56_out1 = burn::tensor::Tensor::cat(
            [neg26_out1, slice87_out1].into(),
            3,
        );
        let mul143_out1 = concat56_out1.mul(unsqueeze14_out1);
        let add86_out1 = mul142_out1.add(mul143_out1);
        let shape72_out1: [i64; 4] = {
            let axes = &add85_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice89_out1: [i64; 1] = shape72_out1[3..4].try_into().unwrap();
        let cast95_out1 = {
            let shape_array = slice89_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt37_out1 = cast95_out1.sqrt();
        let constant778_out1 = self.constant778.val();
        let div63_out1 = constant778_out1.div(sqrt37_out1);
        let transpose41_out1 = add86_out1.permute([0, 1, 3, 2]);
        let sqrt38_out1 = div63_out1.sqrt();
        let mul144_out1 = add85_out1
            .mul((sqrt38_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul145_out1 = transpose41_out1
            .mul((sqrt38_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul76_out1 = mul144_out1.matmul(mul145_out1);
        let add87_out1 = matmul76_out1.add(where2_out1);
        let softmax13_out1 = burn::tensor::activation::softmax(add87_out1, 3);
        let matmul77_out1 = softmax13_out1.matmul(squeeze39_out1);
        let transpose42_out1 = matmul77_out1.permute([0, 2, 1, 3]);
        let unsqueeze94_out1 = [gather55_out1 as i64];
        let constant781_out1: [i64; 1] = [768i64];
        let constant780_out1: [i64; 1] = [-1i64];
        let concat57_out1: [i64; 3usize] = [
            &unsqueeze94_out1[..],
            &constant780_out1[..],
            &constant781_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape29_out1 = transpose42_out1.reshape(concat57_out1);
        let linear50_out1 = self.linear50.forward(reshape29_out1);
        let add88_out1 = add84_out1.add(linear50_out1);
        add88_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule14 {
    layernormalization26: LayerNorm,
    linear51: Linear,
    constant789: burn::module::Param<Tensor<1>>,
    constant790: burn::module::Param<Tensor<1>>,
    constant791: burn::module::Param<Tensor<1>>,
    linear52: Linear,
    layernormalization27: LayerNorm,
    linear53: Linear,
    constant825: burn::module::Param<Tensor<1>>,
    linear54: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule14 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization26 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear51 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant789: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant790: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant791: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear52 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization27 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear53 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant825: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear54 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization26,
            linear51,
            constant789,
            constant790,
            constant791,
            linear52,
            layernormalization27,
            linear53,
            constant825,
            linear54,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add88_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization26_out1 = {
            let dtype = add88_out1.clone().dtype();
            self.layernormalization26
                .forward(add88_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear51_out1 = self.linear51.forward(layernormalization26_out1);
        let shape73_out1: [i64; 3] = {
            let axes = &linear51_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant783_out1: [i64; 1] = [-1i64];
        let gather58_out1: [i64; 1usize] = constant783_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape73_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape73_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant785_out1: [i64; 1] = [1i64];
        let add89_out1 = {
            let __lhs = gather58_out1;
            let __rhs = constant785_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant786_out1: [i64; 1] = [2i64];
        let div64_out1 = {
            let __lhs = add89_out1;
            let __rhs = constant786_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice90_out1 = linear51_out1.clone().slice(s![.., .., 0..div64_out1[0]]);
        let constant788_out1: [i64; 1] = [2i64];
        let mul147_out1 = {
            let __lhs = div64_out1;
            let __rhs = constant788_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice91_out1 = linear51_out1
            .slice(s![.., .., div64_out1[0]..mul147_out1[0]]);
        let constant789_out1 = self.constant789.val();
        let div65_out1 = slice90_out1
            .clone()
            .div((constant789_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf13_out1 = div65_out1.erf();
        let constant790_out1 = self.constant790.val();
        let add90_out1 = erf13_out1
            .add((constant790_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul148_out1 = slice90_out1.mul(add90_out1);
        let constant791_out1 = self.constant791.val();
        let mul149_out1 = mul148_out1
            .mul((constant791_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul150_out1 = mul149_out1.mul(slice91_out1);
        let linear52_out1 = self.linear52.forward(mul150_out1);
        let add91_out1 = add88_out1.add(linear52_out1);
        let layernormalization27_out1 = {
            let dtype = add91_out1.clone().dtype();
            self.layernormalization27
                .forward(add91_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear53_out1 = self.linear53.forward(layernormalization27_out1.clone());
        let shape74_out1: [i64; 3] = {
            let axes = &layernormalization27_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather59_out1 = shape74_out1[0] as i64;
        let unsqueeze95_out1 = [gather59_out1 as i64];
        let constant798_out1: [i64; 1] = [64i64];
        let constant795_out1: [i64; 1] = [-1i64];
        let constant796_out1: [i64; 1] = [3i64];
        let constant797_out1: [i64; 1] = [12i64];
        let concat58_out1: [i64; 5usize] = [
            &unsqueeze95_out1[..],
            &constant795_out1[..],
            &constant796_out1[..],
            &constant797_out1[..],
            &constant798_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape30_out1 = linear53_out1.reshape(concat58_out1);
        let transpose43_out1 = reshape30_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose43_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split14_out1, split14_out2, split14_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze40_out1 = split14_out1.squeeze_dims::<4>(&[2]);
        let squeeze41_out1 = split14_out2.squeeze_dims::<4>(&[2]);
        let squeeze42_out1 = split14_out3.squeeze_dims::<4>(&[2]);
        let mul151_out1 = squeeze40_out1.clone().mul(unsqueeze22_out1.clone());
        let shape75_out1: [i64; 4] = {
            let axes = &squeeze40_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather60_out1 = shape75_out1[3] as i64;
        let constant804_out1 = 2i64;
        let div66_out1 = gather60_out1 / constant804_out1;
        let unsqueeze96_out1 = [div66_out1 as i64];
        let slice92_out1 = squeeze40_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze96_out1[0]]);
        let unsqueeze97_out1 = [div66_out1 as i64];
        let slice93_out1 = squeeze40_out1
            .slice(s![.., .., .., unsqueeze97_out1[0]..9223372036854775807]);
        let neg27_out1 = slice93_out1.neg();
        let concat59_out1 = burn::tensor::Tensor::cat(
            [neg27_out1, slice92_out1].into(),
            3,
        );
        let mul152_out1 = concat59_out1.mul(unsqueeze23_out1.clone());
        let add92_out1 = mul151_out1.add(mul152_out1);
        let mul153_out1 = squeeze41_out1.clone().mul(unsqueeze22_out1);
        let shape76_out1: [i64; 4] = {
            let axes = &squeeze41_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather61_out1 = shape76_out1[3] as i64;
        let constant814_out1 = 2i64;
        let div67_out1 = gather61_out1 / constant814_out1;
        let unsqueeze98_out1 = [div67_out1 as i64];
        let slice94_out1 = squeeze41_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze98_out1[0]]);
        let unsqueeze99_out1 = [div67_out1 as i64];
        let slice95_out1 = squeeze41_out1
            .slice(s![.., .., .., unsqueeze99_out1[0]..9223372036854775807]);
        let neg28_out1 = slice95_out1.neg();
        let concat60_out1 = burn::tensor::Tensor::cat(
            [neg28_out1, slice94_out1].into(),
            3,
        );
        let mul154_out1 = concat60_out1.mul(unsqueeze23_out1);
        let add93_out1 = mul153_out1.add(mul154_out1);
        let shape77_out1: [i64; 4] = {
            let axes = &add92_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice96_out1: [i64; 1] = shape77_out1[3..4].try_into().unwrap();
        let cast101_out1 = {
            let shape_array = slice96_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt40_out1 = cast101_out1.sqrt();
        let constant825_out1 = self.constant825.val();
        let div68_out1 = constant825_out1.div(sqrt40_out1);
        let transpose44_out1 = add93_out1.permute([0, 1, 3, 2]);
        let sqrt41_out1 = div68_out1.sqrt();
        let mul155_out1 = add92_out1
            .mul((sqrt41_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul156_out1 = transpose44_out1
            .mul((sqrt41_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul82_out1 = mul155_out1.matmul(mul156_out1);
        let add94_out1 = matmul82_out1.add(where3_out1);
        let softmax14_out1 = burn::tensor::activation::softmax(add94_out1, 3);
        let matmul83_out1 = softmax14_out1.matmul(squeeze42_out1);
        let transpose45_out1 = matmul83_out1.permute([0, 2, 1, 3]);
        let unsqueeze100_out1 = [gather59_out1 as i64];
        let constant828_out1: [i64; 1] = [768i64];
        let constant827_out1: [i64; 1] = [-1i64];
        let concat61_out1: [i64; 3usize] = [
            &unsqueeze100_out1[..],
            &constant827_out1[..],
            &constant828_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape31_out1 = transpose45_out1.reshape(concat61_out1);
        let linear54_out1 = self.linear54.forward(reshape31_out1);
        let add95_out1 = add91_out1.add(linear54_out1);
        add95_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule15 {
    layernormalization28: LayerNorm,
    linear55: Linear,
    constant836: burn::module::Param<Tensor<1>>,
    constant837: burn::module::Param<Tensor<1>>,
    constant838: burn::module::Param<Tensor<1>>,
    linear56: Linear,
    layernormalization29: LayerNorm,
    linear57: Linear,
    constant872: burn::module::Param<Tensor<1>>,
    linear58: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule15 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization28 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear55 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant836: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant837: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant838: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear56 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization29 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear57 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant872: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear58 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization28,
            linear55,
            constant836,
            constant837,
            constant838,
            linear56,
            layernormalization29,
            linear57,
            constant872,
            linear58,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add95_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization28_out1 = {
            let dtype = add95_out1.clone().dtype();
            self.layernormalization28
                .forward(add95_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear55_out1 = self.linear55.forward(layernormalization28_out1);
        let shape78_out1: [i64; 3] = {
            let axes = &linear55_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant830_out1: [i64; 1] = [-1i64];
        let gather62_out1: [i64; 1usize] = constant830_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape78_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape78_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant832_out1: [i64; 1] = [1i64];
        let add96_out1 = {
            let __lhs = gather62_out1;
            let __rhs = constant832_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant833_out1: [i64; 1] = [2i64];
        let div69_out1 = {
            let __lhs = add96_out1;
            let __rhs = constant833_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice97_out1 = linear55_out1.clone().slice(s![.., .., 0..div69_out1[0]]);
        let constant835_out1: [i64; 1] = [2i64];
        let mul158_out1 = {
            let __lhs = div69_out1;
            let __rhs = constant835_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice98_out1 = linear55_out1
            .slice(s![.., .., div69_out1[0]..mul158_out1[0]]);
        let constant836_out1 = self.constant836.val();
        let div70_out1 = slice97_out1
            .clone()
            .div((constant836_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf14_out1 = div70_out1.erf();
        let constant837_out1 = self.constant837.val();
        let add97_out1 = erf14_out1
            .add((constant837_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul159_out1 = slice97_out1.mul(add97_out1);
        let constant838_out1 = self.constant838.val();
        let mul160_out1 = mul159_out1
            .mul((constant838_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul161_out1 = mul160_out1.mul(slice98_out1);
        let linear56_out1 = self.linear56.forward(mul161_out1);
        let add98_out1 = add95_out1.add(linear56_out1);
        let layernormalization29_out1 = {
            let dtype = add98_out1.clone().dtype();
            self.layernormalization29
                .forward(add98_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear57_out1 = self.linear57.forward(layernormalization29_out1.clone());
        let shape79_out1: [i64; 3] = {
            let axes = &layernormalization29_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather63_out1 = shape79_out1[0] as i64;
        let unsqueeze101_out1 = [gather63_out1 as i64];
        let constant845_out1: [i64; 1] = [64i64];
        let constant842_out1: [i64; 1] = [-1i64];
        let constant843_out1: [i64; 1] = [3i64];
        let constant844_out1: [i64; 1] = [12i64];
        let concat62_out1: [i64; 5usize] = [
            &unsqueeze101_out1[..],
            &constant842_out1[..],
            &constant843_out1[..],
            &constant844_out1[..],
            &constant845_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape32_out1 = linear57_out1.reshape(concat62_out1);
        let transpose46_out1 = reshape32_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose46_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split15_out1, split15_out2, split15_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze43_out1 = split15_out1.squeeze_dims::<4>(&[2]);
        let squeeze44_out1 = split15_out2.squeeze_dims::<4>(&[2]);
        let squeeze45_out1 = split15_out3.squeeze_dims::<4>(&[2]);
        let mul162_out1 = squeeze43_out1.clone().mul(unsqueeze22_out1.clone());
        let shape80_out1: [i64; 4] = {
            let axes = &squeeze43_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather64_out1 = shape80_out1[3] as i64;
        let constant851_out1 = 2i64;
        let div71_out1 = gather64_out1 / constant851_out1;
        let unsqueeze102_out1 = [div71_out1 as i64];
        let slice99_out1 = squeeze43_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze102_out1[0]]);
        let unsqueeze103_out1 = [div71_out1 as i64];
        let slice100_out1 = squeeze43_out1
            .slice(s![.., .., .., unsqueeze103_out1[0]..9223372036854775807]);
        let neg29_out1 = slice100_out1.neg();
        let concat63_out1 = burn::tensor::Tensor::cat(
            [neg29_out1, slice99_out1].into(),
            3,
        );
        let mul163_out1 = concat63_out1.mul(unsqueeze23_out1.clone());
        let add99_out1 = mul162_out1.add(mul163_out1);
        let mul164_out1 = squeeze44_out1.clone().mul(unsqueeze22_out1);
        let shape81_out1: [i64; 4] = {
            let axes = &squeeze44_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather65_out1 = shape81_out1[3] as i64;
        let constant861_out1 = 2i64;
        let div72_out1 = gather65_out1 / constant861_out1;
        let unsqueeze104_out1 = [div72_out1 as i64];
        let slice101_out1 = squeeze44_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze104_out1[0]]);
        let unsqueeze105_out1 = [div72_out1 as i64];
        let slice102_out1 = squeeze44_out1
            .slice(s![.., .., .., unsqueeze105_out1[0]..9223372036854775807]);
        let neg30_out1 = slice102_out1.neg();
        let concat64_out1 = burn::tensor::Tensor::cat(
            [neg30_out1, slice101_out1].into(),
            3,
        );
        let mul165_out1 = concat64_out1.mul(unsqueeze23_out1);
        let add100_out1 = mul164_out1.add(mul165_out1);
        let shape82_out1: [i64; 4] = {
            let axes = &add99_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice103_out1: [i64; 1] = shape82_out1[3..4].try_into().unwrap();
        let cast107_out1 = {
            let shape_array = slice103_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt43_out1 = cast107_out1.sqrt();
        let constant872_out1 = self.constant872.val();
        let div73_out1 = constant872_out1.div(sqrt43_out1);
        let transpose47_out1 = add100_out1.permute([0, 1, 3, 2]);
        let sqrt44_out1 = div73_out1.sqrt();
        let mul166_out1 = add99_out1
            .mul((sqrt44_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul167_out1 = transpose47_out1
            .mul((sqrt44_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul88_out1 = mul166_out1.matmul(mul167_out1);
        let add101_out1 = matmul88_out1.add(where3_out1);
        let softmax15_out1 = burn::tensor::activation::softmax(add101_out1, 3);
        let matmul89_out1 = softmax15_out1.matmul(squeeze45_out1);
        let transpose48_out1 = matmul89_out1.permute([0, 2, 1, 3]);
        let unsqueeze106_out1 = [gather63_out1 as i64];
        let constant875_out1: [i64; 1] = [768i64];
        let constant874_out1: [i64; 1] = [-1i64];
        let concat65_out1: [i64; 3usize] = [
            &unsqueeze106_out1[..],
            &constant874_out1[..],
            &constant875_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape33_out1 = transpose48_out1.reshape(concat65_out1);
        let linear58_out1 = self.linear58.forward(reshape33_out1);
        let add102_out1 = add98_out1.add(linear58_out1);
        add102_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule16 {
    layernormalization30: LayerNorm,
    linear59: Linear,
    constant883: burn::module::Param<Tensor<1>>,
    constant884: burn::module::Param<Tensor<1>>,
    constant885: burn::module::Param<Tensor<1>>,
    linear60: Linear,
    layernormalization31: LayerNorm,
    linear61: Linear,
    constant919: burn::module::Param<Tensor<1>>,
    linear62: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule16 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization30 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear59 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant883: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant884: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant885: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear60 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization31 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear61 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant919: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear62 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization30,
            linear59,
            constant883,
            constant884,
            constant885,
            linear60,
            layernormalization31,
            linear61,
            constant919,
            linear62,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add102_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization30_out1 = {
            let dtype = add102_out1.clone().dtype();
            self.layernormalization30
                .forward(add102_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear59_out1 = self.linear59.forward(layernormalization30_out1);
        let shape83_out1: [i64; 3] = {
            let axes = &linear59_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant877_out1: [i64; 1] = [-1i64];
        let gather66_out1: [i64; 1usize] = constant877_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape83_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape83_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant879_out1: [i64; 1] = [1i64];
        let add103_out1 = {
            let __lhs = gather66_out1;
            let __rhs = constant879_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant880_out1: [i64; 1] = [2i64];
        let div74_out1 = {
            let __lhs = add103_out1;
            let __rhs = constant880_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice104_out1 = linear59_out1.clone().slice(s![.., .., 0..div74_out1[0]]);
        let constant882_out1: [i64; 1] = [2i64];
        let mul169_out1 = {
            let __lhs = div74_out1;
            let __rhs = constant882_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice105_out1 = linear59_out1
            .slice(s![.., .., div74_out1[0]..mul169_out1[0]]);
        let constant883_out1 = self.constant883.val();
        let div75_out1 = slice104_out1
            .clone()
            .div((constant883_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf15_out1 = div75_out1.erf();
        let constant884_out1 = self.constant884.val();
        let add104_out1 = erf15_out1
            .add((constant884_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul170_out1 = slice104_out1.mul(add104_out1);
        let constant885_out1 = self.constant885.val();
        let mul171_out1 = mul170_out1
            .mul((constant885_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul172_out1 = mul171_out1.mul(slice105_out1);
        let linear60_out1 = self.linear60.forward(mul172_out1);
        let add105_out1 = add102_out1.add(linear60_out1);
        let layernormalization31_out1 = {
            let dtype = add105_out1.clone().dtype();
            self.layernormalization31
                .forward(add105_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear61_out1 = self.linear61.forward(layernormalization31_out1.clone());
        let shape84_out1: [i64; 3] = {
            let axes = &layernormalization31_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather67_out1 = shape84_out1[0] as i64;
        let unsqueeze107_out1 = [gather67_out1 as i64];
        let constant892_out1: [i64; 1] = [64i64];
        let constant889_out1: [i64; 1] = [-1i64];
        let constant890_out1: [i64; 1] = [3i64];
        let constant891_out1: [i64; 1] = [12i64];
        let concat66_out1: [i64; 5usize] = [
            &unsqueeze107_out1[..],
            &constant889_out1[..],
            &constant890_out1[..],
            &constant891_out1[..],
            &constant892_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape34_out1 = linear61_out1.reshape(concat66_out1);
        let transpose49_out1 = reshape34_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose49_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split16_out1, split16_out2, split16_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze46_out1 = split16_out1.squeeze_dims::<4>(&[2]);
        let squeeze47_out1 = split16_out2.squeeze_dims::<4>(&[2]);
        let squeeze48_out1 = split16_out3.squeeze_dims::<4>(&[2]);
        let mul173_out1 = squeeze46_out1.clone().mul(unsqueeze13_out1.clone());
        let shape85_out1: [i64; 4] = {
            let axes = &squeeze46_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather68_out1 = shape85_out1[3] as i64;
        let constant898_out1 = 2i64;
        let div76_out1 = gather68_out1 / constant898_out1;
        let unsqueeze108_out1 = [div76_out1 as i64];
        let slice106_out1 = squeeze46_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze108_out1[0]]);
        let unsqueeze109_out1 = [div76_out1 as i64];
        let slice107_out1 = squeeze46_out1
            .slice(s![.., .., .., unsqueeze109_out1[0]..9223372036854775807]);
        let neg31_out1 = slice107_out1.neg();
        let concat67_out1 = burn::tensor::Tensor::cat(
            [neg31_out1, slice106_out1].into(),
            3,
        );
        let mul174_out1 = concat67_out1.mul(unsqueeze14_out1.clone());
        let add106_out1 = mul173_out1.add(mul174_out1);
        let mul175_out1 = squeeze47_out1.clone().mul(unsqueeze13_out1);
        let shape86_out1: [i64; 4] = {
            let axes = &squeeze47_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather69_out1 = shape86_out1[3] as i64;
        let constant908_out1 = 2i64;
        let div77_out1 = gather69_out1 / constant908_out1;
        let unsqueeze110_out1 = [div77_out1 as i64];
        let slice108_out1 = squeeze47_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze110_out1[0]]);
        let unsqueeze111_out1 = [div77_out1 as i64];
        let slice109_out1 = squeeze47_out1
            .slice(s![.., .., .., unsqueeze111_out1[0]..9223372036854775807]);
        let neg32_out1 = slice109_out1.neg();
        let concat68_out1 = burn::tensor::Tensor::cat(
            [neg32_out1, slice108_out1].into(),
            3,
        );
        let mul176_out1 = concat68_out1.mul(unsqueeze14_out1);
        let add107_out1 = mul175_out1.add(mul176_out1);
        let shape87_out1: [i64; 4] = {
            let axes = &add106_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice110_out1: [i64; 1] = shape87_out1[3..4].try_into().unwrap();
        let cast113_out1 = {
            let shape_array = slice110_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt46_out1 = cast113_out1.sqrt();
        let constant919_out1 = self.constant919.val();
        let div78_out1 = constant919_out1.div(sqrt46_out1);
        let transpose50_out1 = add107_out1.permute([0, 1, 3, 2]);
        let sqrt47_out1 = div78_out1.sqrt();
        let mul177_out1 = add106_out1
            .mul((sqrt47_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul178_out1 = transpose50_out1
            .mul((sqrt47_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul94_out1 = mul177_out1.matmul(mul178_out1);
        let add108_out1 = matmul94_out1.add(where2_out1);
        let softmax16_out1 = burn::tensor::activation::softmax(add108_out1, 3);
        let matmul95_out1 = softmax16_out1.matmul(squeeze48_out1);
        let transpose51_out1 = matmul95_out1.permute([0, 2, 1, 3]);
        let unsqueeze112_out1 = [gather67_out1 as i64];
        let constant922_out1: [i64; 1] = [768i64];
        let constant921_out1: [i64; 1] = [-1i64];
        let concat69_out1: [i64; 3usize] = [
            &unsqueeze112_out1[..],
            &constant921_out1[..],
            &constant922_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape35_out1 = transpose51_out1.reshape(concat69_out1);
        let linear62_out1 = self.linear62.forward(reshape35_out1);
        let add109_out1 = add105_out1.add(linear62_out1);
        add109_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule17 {
    layernormalization32: LayerNorm,
    linear63: Linear,
    constant930: burn::module::Param<Tensor<1>>,
    constant931: burn::module::Param<Tensor<1>>,
    constant932: burn::module::Param<Tensor<1>>,
    linear64: Linear,
    layernormalization33: LayerNorm,
    linear65: Linear,
    constant966: burn::module::Param<Tensor<1>>,
    linear66: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule17 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization32 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear63 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant930: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant931: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant932: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear64 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization33 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear65 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant966: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear66 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization32,
            linear63,
            constant930,
            constant931,
            constant932,
            linear64,
            layernormalization33,
            linear65,
            constant966,
            linear66,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add109_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization32_out1 = {
            let dtype = add109_out1.clone().dtype();
            self.layernormalization32
                .forward(add109_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear63_out1 = self.linear63.forward(layernormalization32_out1);
        let shape88_out1: [i64; 3] = {
            let axes = &linear63_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant924_out1: [i64; 1] = [-1i64];
        let gather70_out1: [i64; 1usize] = constant924_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape88_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape88_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant926_out1: [i64; 1] = [1i64];
        let add110_out1 = {
            let __lhs = gather70_out1;
            let __rhs = constant926_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant927_out1: [i64; 1] = [2i64];
        let div79_out1 = {
            let __lhs = add110_out1;
            let __rhs = constant927_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice111_out1 = linear63_out1.clone().slice(s![.., .., 0..div79_out1[0]]);
        let constant929_out1: [i64; 1] = [2i64];
        let mul180_out1 = {
            let __lhs = div79_out1;
            let __rhs = constant929_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice112_out1 = linear63_out1
            .slice(s![.., .., div79_out1[0]..mul180_out1[0]]);
        let constant930_out1 = self.constant930.val();
        let div80_out1 = slice111_out1
            .clone()
            .div((constant930_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf16_out1 = div80_out1.erf();
        let constant931_out1 = self.constant931.val();
        let add111_out1 = erf16_out1
            .add((constant931_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul181_out1 = slice111_out1.mul(add111_out1);
        let constant932_out1 = self.constant932.val();
        let mul182_out1 = mul181_out1
            .mul((constant932_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul183_out1 = mul182_out1.mul(slice112_out1);
        let linear64_out1 = self.linear64.forward(mul183_out1);
        let add112_out1 = add109_out1.add(linear64_out1);
        let layernormalization33_out1 = {
            let dtype = add112_out1.clone().dtype();
            self.layernormalization33
                .forward(add112_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear65_out1 = self.linear65.forward(layernormalization33_out1.clone());
        let shape89_out1: [i64; 3] = {
            let axes = &layernormalization33_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather71_out1 = shape89_out1[0] as i64;
        let unsqueeze113_out1 = [gather71_out1 as i64];
        let constant939_out1: [i64; 1] = [64i64];
        let constant936_out1: [i64; 1] = [-1i64];
        let constant937_out1: [i64; 1] = [3i64];
        let constant938_out1: [i64; 1] = [12i64];
        let concat70_out1: [i64; 5usize] = [
            &unsqueeze113_out1[..],
            &constant936_out1[..],
            &constant937_out1[..],
            &constant938_out1[..],
            &constant939_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape36_out1 = linear65_out1.reshape(concat70_out1);
        let transpose52_out1 = reshape36_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose52_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split17_out1, split17_out2, split17_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze49_out1 = split17_out1.squeeze_dims::<4>(&[2]);
        let squeeze50_out1 = split17_out2.squeeze_dims::<4>(&[2]);
        let squeeze51_out1 = split17_out3.squeeze_dims::<4>(&[2]);
        let mul184_out1 = squeeze49_out1.clone().mul(unsqueeze22_out1.clone());
        let shape90_out1: [i64; 4] = {
            let axes = &squeeze49_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather72_out1 = shape90_out1[3] as i64;
        let constant945_out1 = 2i64;
        let div81_out1 = gather72_out1 / constant945_out1;
        let unsqueeze114_out1 = [div81_out1 as i64];
        let slice113_out1 = squeeze49_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze114_out1[0]]);
        let unsqueeze115_out1 = [div81_out1 as i64];
        let slice114_out1 = squeeze49_out1
            .slice(s![.., .., .., unsqueeze115_out1[0]..9223372036854775807]);
        let neg33_out1 = slice114_out1.neg();
        let concat71_out1 = burn::tensor::Tensor::cat(
            [neg33_out1, slice113_out1].into(),
            3,
        );
        let mul185_out1 = concat71_out1.mul(unsqueeze23_out1.clone());
        let add113_out1 = mul184_out1.add(mul185_out1);
        let mul186_out1 = squeeze50_out1.clone().mul(unsqueeze22_out1);
        let shape91_out1: [i64; 4] = {
            let axes = &squeeze50_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather73_out1 = shape91_out1[3] as i64;
        let constant955_out1 = 2i64;
        let div82_out1 = gather73_out1 / constant955_out1;
        let unsqueeze116_out1 = [div82_out1 as i64];
        let slice115_out1 = squeeze50_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze116_out1[0]]);
        let unsqueeze117_out1 = [div82_out1 as i64];
        let slice116_out1 = squeeze50_out1
            .slice(s![.., .., .., unsqueeze117_out1[0]..9223372036854775807]);
        let neg34_out1 = slice116_out1.neg();
        let concat72_out1 = burn::tensor::Tensor::cat(
            [neg34_out1, slice115_out1].into(),
            3,
        );
        let mul187_out1 = concat72_out1.mul(unsqueeze23_out1);
        let add114_out1 = mul186_out1.add(mul187_out1);
        let shape92_out1: [i64; 4] = {
            let axes = &add113_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice117_out1: [i64; 1] = shape92_out1[3..4].try_into().unwrap();
        let cast119_out1 = {
            let shape_array = slice117_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt49_out1 = cast119_out1.sqrt();
        let constant966_out1 = self.constant966.val();
        let div83_out1 = constant966_out1.div(sqrt49_out1);
        let transpose53_out1 = add114_out1.permute([0, 1, 3, 2]);
        let sqrt50_out1 = div83_out1.sqrt();
        let mul188_out1 = add113_out1
            .mul((sqrt50_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul189_out1 = transpose53_out1
            .mul((sqrt50_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul100_out1 = mul188_out1.matmul(mul189_out1);
        let add115_out1 = matmul100_out1.add(where3_out1);
        let softmax17_out1 = burn::tensor::activation::softmax(add115_out1, 3);
        let matmul101_out1 = softmax17_out1.matmul(squeeze51_out1);
        let transpose54_out1 = matmul101_out1.permute([0, 2, 1, 3]);
        let unsqueeze118_out1 = [gather71_out1 as i64];
        let constant969_out1: [i64; 1] = [768i64];
        let constant968_out1: [i64; 1] = [-1i64];
        let concat73_out1: [i64; 3usize] = [
            &unsqueeze118_out1[..],
            &constant968_out1[..],
            &constant969_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape37_out1 = transpose54_out1.reshape(concat73_out1);
        let linear66_out1 = self.linear66.forward(reshape37_out1);
        let add116_out1 = add112_out1.add(linear66_out1);
        add116_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule18 {
    layernormalization34: LayerNorm,
    linear67: Linear,
    constant977: burn::module::Param<Tensor<1>>,
    constant978: burn::module::Param<Tensor<1>>,
    constant979: burn::module::Param<Tensor<1>>,
    linear68: Linear,
    layernormalization35: LayerNorm,
    linear69: Linear,
    constant1013: burn::module::Param<Tensor<1>>,
    linear70: Linear,
    #[module(skip)]
    device: Device,
}
impl Submodule18 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization34 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear67 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant977: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant978: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant979: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear68 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization35 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear69 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1013: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear70 = LinearConfig::new(768, 768).with_bias(false).init(device);
        Self {
            layernormalization34,
            linear67,
            constant977,
            constant978,
            constant979,
            linear68,
            layernormalization35,
            linear69,
            constant1013,
            linear70,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add116_out1: Tensor<3>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> Tensor<3> {
        let layernormalization34_out1 = {
            let dtype = add116_out1.clone().dtype();
            self.layernormalization34
                .forward(add116_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear67_out1 = self.linear67.forward(layernormalization34_out1);
        let shape93_out1: [i64; 3] = {
            let axes = &linear67_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant971_out1: [i64; 1] = [-1i64];
        let gather74_out1: [i64; 1usize] = constant971_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape93_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape93_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant973_out1: [i64; 1] = [1i64];
        let add117_out1 = {
            let __lhs = gather74_out1;
            let __rhs = constant973_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant974_out1: [i64; 1] = [2i64];
        let div84_out1 = {
            let __lhs = add117_out1;
            let __rhs = constant974_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice118_out1 = linear67_out1.clone().slice(s![.., .., 0..div84_out1[0]]);
        let constant976_out1: [i64; 1] = [2i64];
        let mul191_out1 = {
            let __lhs = div84_out1;
            let __rhs = constant976_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice119_out1 = linear67_out1
            .slice(s![.., .., div84_out1[0]..mul191_out1[0]]);
        let constant977_out1 = self.constant977.val();
        let div85_out1 = slice118_out1
            .clone()
            .div((constant977_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf17_out1 = div85_out1.erf();
        let constant978_out1 = self.constant978.val();
        let add118_out1 = erf17_out1
            .add((constant978_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul192_out1 = slice118_out1.mul(add118_out1);
        let constant979_out1 = self.constant979.val();
        let mul193_out1 = mul192_out1
            .mul((constant979_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul194_out1 = mul193_out1.mul(slice119_out1);
        let linear68_out1 = self.linear68.forward(mul194_out1);
        let add119_out1 = add116_out1.add(linear68_out1);
        let layernormalization35_out1 = {
            let dtype = add119_out1.clone().dtype();
            self.layernormalization35
                .forward(add119_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear69_out1 = self.linear69.forward(layernormalization35_out1.clone());
        let shape94_out1: [i64; 3] = {
            let axes = &layernormalization35_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather75_out1 = shape94_out1[0] as i64;
        let unsqueeze119_out1 = [gather75_out1 as i64];
        let constant986_out1: [i64; 1] = [64i64];
        let constant983_out1: [i64; 1] = [-1i64];
        let constant984_out1: [i64; 1] = [3i64];
        let constant985_out1: [i64; 1] = [12i64];
        let concat74_out1: [i64; 5usize] = [
            &unsqueeze119_out1[..],
            &constant983_out1[..],
            &constant984_out1[..],
            &constant985_out1[..],
            &constant986_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape38_out1 = linear69_out1.reshape(concat74_out1);
        let transpose55_out1 = reshape38_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose55_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split18_out1, split18_out2, split18_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze52_out1 = split18_out1.squeeze_dims::<4>(&[2]);
        let squeeze53_out1 = split18_out2.squeeze_dims::<4>(&[2]);
        let squeeze54_out1 = split18_out3.squeeze_dims::<4>(&[2]);
        let mul195_out1 = squeeze52_out1.clone().mul(unsqueeze22_out1.clone());
        let shape95_out1: [i64; 4] = {
            let axes = &squeeze52_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather76_out1 = shape95_out1[3] as i64;
        let constant992_out1 = 2i64;
        let div86_out1 = gather76_out1 / constant992_out1;
        let unsqueeze120_out1 = [div86_out1 as i64];
        let slice120_out1 = squeeze52_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze120_out1[0]]);
        let unsqueeze121_out1 = [div86_out1 as i64];
        let slice121_out1 = squeeze52_out1
            .slice(s![.., .., .., unsqueeze121_out1[0]..9223372036854775807]);
        let neg35_out1 = slice121_out1.neg();
        let concat75_out1 = burn::tensor::Tensor::cat(
            [neg35_out1, slice120_out1].into(),
            3,
        );
        let mul196_out1 = concat75_out1.mul(unsqueeze23_out1.clone());
        let add120_out1 = mul195_out1.add(mul196_out1);
        let mul197_out1 = squeeze53_out1.clone().mul(unsqueeze22_out1);
        let shape96_out1: [i64; 4] = {
            let axes = &squeeze53_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather77_out1 = shape96_out1[3] as i64;
        let constant1002_out1 = 2i64;
        let div87_out1 = gather77_out1 / constant1002_out1;
        let unsqueeze122_out1 = [div87_out1 as i64];
        let slice122_out1 = squeeze53_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze122_out1[0]]);
        let unsqueeze123_out1 = [div87_out1 as i64];
        let slice123_out1 = squeeze53_out1
            .slice(s![.., .., .., unsqueeze123_out1[0]..9223372036854775807]);
        let neg36_out1 = slice123_out1.neg();
        let concat76_out1 = burn::tensor::Tensor::cat(
            [neg36_out1, slice122_out1].into(),
            3,
        );
        let mul198_out1 = concat76_out1.mul(unsqueeze23_out1);
        let add121_out1 = mul197_out1.add(mul198_out1);
        let shape97_out1: [i64; 4] = {
            let axes = &add120_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice124_out1: [i64; 1] = shape97_out1[3..4].try_into().unwrap();
        let cast125_out1 = {
            let shape_array = slice124_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt52_out1 = cast125_out1.sqrt();
        let constant1013_out1 = self.constant1013.val();
        let div88_out1 = constant1013_out1.div(sqrt52_out1);
        let transpose56_out1 = add121_out1.permute([0, 1, 3, 2]);
        let sqrt53_out1 = div88_out1.sqrt();
        let mul199_out1 = add120_out1
            .mul((sqrt53_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul200_out1 = transpose56_out1
            .mul((sqrt53_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul106_out1 = mul199_out1.matmul(mul200_out1);
        let add122_out1 = matmul106_out1.add(where3_out1);
        let softmax18_out1 = burn::tensor::activation::softmax(add122_out1, 3);
        let matmul107_out1 = softmax18_out1.matmul(squeeze54_out1);
        let transpose57_out1 = matmul107_out1.permute([0, 2, 1, 3]);
        let unsqueeze124_out1 = [gather75_out1 as i64];
        let constant1016_out1: [i64; 1] = [768i64];
        let constant1015_out1: [i64; 1] = [-1i64];
        let concat77_out1: [i64; 3usize] = [
            &unsqueeze124_out1[..],
            &constant1015_out1[..],
            &constant1016_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape39_out1 = transpose57_out1.reshape(concat77_out1);
        let linear70_out1 = self.linear70.forward(reshape39_out1);
        let add123_out1 = add119_out1.add(linear70_out1);
        add123_out1
    }
}
#[derive(Module, Debug)]
pub struct Submodule19 {
    layernormalization36: LayerNorm,
    linear71: Linear,
    constant1024: burn::module::Param<Tensor<1>>,
    constant1025: burn::module::Param<Tensor<1>>,
    constant1026: burn::module::Param<Tensor<1>>,
    linear72: Linear,
    layernormalization37: LayerNorm,
    linear73: Linear,
    constant1060: burn::module::Param<Tensor<1>>,
    linear74: Linear,
    layernormalization38: LayerNorm,
    linear75: Linear,
    constant1071: burn::module::Param<Tensor<1>>,
    constant1072: burn::module::Param<Tensor<1>>,
    constant1073: burn::module::Param<Tensor<1>>,
    linear76: Linear,
    layernormalization39: LayerNorm,
    linear77: Linear,
    constant1107: burn::module::Param<Tensor<1>>,
    linear78: Linear,
    layernormalization40: LayerNorm,
    linear79: Linear,
    constant1118: burn::module::Param<Tensor<1>>,
    constant1119: burn::module::Param<Tensor<1>>,
    constant1120: burn::module::Param<Tensor<1>>,
    linear80: Linear,
    layernormalization41: LayerNorm,
    linear81: Linear,
    constant1154: burn::module::Param<Tensor<1>>,
    #[module(skip)]
    device: Device,
}
impl Submodule19 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let layernormalization36 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear71 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1024: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1025: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1026: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear72 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization37 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear73 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1060: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear74 = LinearConfig::new(768, 768).with_bias(false).init(device);
        let layernormalization38 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear75 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1071: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1072: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1073: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear76 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization39 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear77 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1107: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear78 = LinearConfig::new(768, 768).with_bias(false).init(device);
        let layernormalization40 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear79 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1118: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1119: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1120: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear80 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization41 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear81 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1154: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        Self {
            layernormalization36,
            linear71,
            constant1024,
            constant1025,
            constant1026,
            linear72,
            layernormalization37,
            linear73,
            constant1060,
            linear74,
            layernormalization38,
            linear75,
            constant1071,
            constant1072,
            constant1073,
            linear76,
            layernormalization39,
            linear77,
            constant1107,
            linear78,
            layernormalization40,
            linear79,
            constant1118,
            constant1119,
            constant1120,
            linear80,
            layernormalization41,
            linear81,
            constant1154,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        add123_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
        unsqueeze22_out1: Tensor<4>,
        unsqueeze23_out1: Tensor<4>,
        where3_out1: Tensor<4>,
    ) -> (Tensor<4>, i64, Tensor<3>) {
        let layernormalization36_out1 = {
            let dtype = add123_out1.clone().dtype();
            self.layernormalization36
                .forward(add123_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear71_out1 = self.linear71.forward(layernormalization36_out1);
        let shape98_out1: [i64; 3] = {
            let axes = &linear71_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant1018_out1: [i64; 1] = [-1i64];
        let gather78_out1: [i64; 1usize] = constant1018_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape98_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape98_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant1020_out1: [i64; 1] = [1i64];
        let add124_out1 = {
            let __lhs = gather78_out1;
            let __rhs = constant1020_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant1021_out1: [i64; 1] = [2i64];
        let div89_out1 = {
            let __lhs = add124_out1;
            let __rhs = constant1021_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice125_out1 = linear71_out1.clone().slice(s![.., .., 0..div89_out1[0]]);
        let constant1023_out1: [i64; 1] = [2i64];
        let mul202_out1 = {
            let __lhs = div89_out1;
            let __rhs = constant1023_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice126_out1 = linear71_out1
            .slice(s![.., .., div89_out1[0]..mul202_out1[0]]);
        let constant1024_out1 = self.constant1024.val();
        let div90_out1 = slice125_out1
            .clone()
            .div((constant1024_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf18_out1 = div90_out1.erf();
        let constant1025_out1 = self.constant1025.val();
        let add125_out1 = erf18_out1
            .add((constant1025_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul203_out1 = slice125_out1.mul(add125_out1);
        let constant1026_out1 = self.constant1026.val();
        let mul204_out1 = mul203_out1
            .mul((constant1026_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul205_out1 = mul204_out1.mul(slice126_out1);
        let linear72_out1 = self.linear72.forward(mul205_out1);
        let add126_out1 = add123_out1.add(linear72_out1);
        let layernormalization37_out1 = {
            let dtype = add126_out1.clone().dtype();
            self.layernormalization37
                .forward(add126_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear73_out1 = self.linear73.forward(layernormalization37_out1.clone());
        let shape99_out1: [i64; 3] = {
            let axes = &layernormalization37_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather79_out1 = shape99_out1[0] as i64;
        let unsqueeze125_out1 = [gather79_out1 as i64];
        let constant1033_out1: [i64; 1] = [64i64];
        let constant1030_out1: [i64; 1] = [-1i64];
        let constant1031_out1: [i64; 1] = [3i64];
        let constant1032_out1: [i64; 1] = [12i64];
        let concat78_out1: [i64; 5usize] = [
            &unsqueeze125_out1[..],
            &constant1030_out1[..],
            &constant1031_out1[..],
            &constant1032_out1[..],
            &constant1033_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape40_out1 = linear73_out1.reshape(concat78_out1);
        let transpose58_out1 = reshape40_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose58_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split19_out1, split19_out2, split19_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze55_out1 = split19_out1.squeeze_dims::<4>(&[2]);
        let squeeze56_out1 = split19_out2.squeeze_dims::<4>(&[2]);
        let squeeze57_out1 = split19_out3.squeeze_dims::<4>(&[2]);
        let mul206_out1 = squeeze55_out1.clone().mul(unsqueeze13_out1.clone());
        let shape100_out1: [i64; 4] = {
            let axes = &squeeze55_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather80_out1 = shape100_out1[3] as i64;
        let constant1039_out1 = 2i64;
        let div91_out1 = gather80_out1 / constant1039_out1;
        let unsqueeze126_out1 = [div91_out1 as i64];
        let slice127_out1 = squeeze55_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze126_out1[0]]);
        let unsqueeze127_out1 = [div91_out1 as i64];
        let slice128_out1 = squeeze55_out1
            .slice(s![.., .., .., unsqueeze127_out1[0]..9223372036854775807]);
        let neg37_out1 = slice128_out1.neg();
        let concat79_out1 = burn::tensor::Tensor::cat(
            [neg37_out1, slice127_out1].into(),
            3,
        );
        let mul207_out1 = concat79_out1.mul(unsqueeze14_out1.clone());
        let add127_out1 = mul206_out1.add(mul207_out1);
        let mul208_out1 = squeeze56_out1.clone().mul(unsqueeze13_out1);
        let shape101_out1: [i64; 4] = {
            let axes = &squeeze56_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather81_out1 = shape101_out1[3] as i64;
        let constant1049_out1 = 2i64;
        let div92_out1 = gather81_out1 / constant1049_out1;
        let unsqueeze128_out1 = [div92_out1 as i64];
        let slice129_out1 = squeeze56_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze128_out1[0]]);
        let unsqueeze129_out1 = [div92_out1 as i64];
        let slice130_out1 = squeeze56_out1
            .slice(s![.., .., .., unsqueeze129_out1[0]..9223372036854775807]);
        let neg38_out1 = slice130_out1.neg();
        let concat80_out1 = burn::tensor::Tensor::cat(
            [neg38_out1, slice129_out1].into(),
            3,
        );
        let mul209_out1 = concat80_out1.mul(unsqueeze14_out1);
        let add128_out1 = mul208_out1.add(mul209_out1);
        let shape102_out1: [i64; 4] = {
            let axes = &add127_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice131_out1: [i64; 1] = shape102_out1[3..4].try_into().unwrap();
        let cast131_out1 = {
            let shape_array = slice131_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt55_out1 = cast131_out1.sqrt();
        let constant1060_out1 = self.constant1060.val();
        let div93_out1 = constant1060_out1.div(sqrt55_out1);
        let transpose59_out1 = add128_out1.permute([0, 1, 3, 2]);
        let sqrt56_out1 = div93_out1.sqrt();
        let mul210_out1 = add127_out1
            .mul((sqrt56_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul211_out1 = transpose59_out1
            .mul((sqrt56_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul112_out1 = mul210_out1.matmul(mul211_out1);
        let add129_out1 = matmul112_out1.add(where2_out1);
        let softmax19_out1 = burn::tensor::activation::softmax(add129_out1, 3);
        let matmul113_out1 = softmax19_out1.matmul(squeeze57_out1);
        let transpose60_out1 = matmul113_out1.permute([0, 2, 1, 3]);
        let unsqueeze130_out1 = [gather79_out1 as i64];
        let constant1063_out1: [i64; 1] = [768i64];
        let constant1062_out1: [i64; 1] = [-1i64];
        let concat81_out1: [i64; 3usize] = [
            &unsqueeze130_out1[..],
            &constant1062_out1[..],
            &constant1063_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape41_out1 = transpose60_out1.reshape(concat81_out1);
        let linear74_out1 = self.linear74.forward(reshape41_out1);
        let add130_out1 = add126_out1.add(linear74_out1);
        let layernormalization38_out1 = {
            let dtype = add130_out1.clone().dtype();
            self.layernormalization38
                .forward(add130_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear75_out1 = self.linear75.forward(layernormalization38_out1);
        let shape103_out1: [i64; 3] = {
            let axes = &linear75_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant1065_out1: [i64; 1] = [-1i64];
        let gather82_out1: [i64; 1usize] = constant1065_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape103_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape103_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant1067_out1: [i64; 1] = [1i64];
        let add131_out1 = {
            let __lhs = gather82_out1;
            let __rhs = constant1067_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant1068_out1: [i64; 1] = [2i64];
        let div94_out1 = {
            let __lhs = add131_out1;
            let __rhs = constant1068_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice132_out1 = linear75_out1.clone().slice(s![.., .., 0..div94_out1[0]]);
        let constant1070_out1: [i64; 1] = [2i64];
        let mul213_out1 = {
            let __lhs = div94_out1;
            let __rhs = constant1070_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice133_out1 = linear75_out1
            .slice(s![.., .., div94_out1[0]..mul213_out1[0]]);
        let constant1071_out1 = self.constant1071.val();
        let div95_out1 = slice132_out1
            .clone()
            .div((constant1071_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf19_out1 = div95_out1.erf();
        let constant1072_out1 = self.constant1072.val();
        let add132_out1 = erf19_out1
            .add((constant1072_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul214_out1 = slice132_out1.mul(add132_out1);
        let constant1073_out1 = self.constant1073.val();
        let mul215_out1 = mul214_out1
            .mul((constant1073_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul216_out1 = mul215_out1.mul(slice133_out1);
        let linear76_out1 = self.linear76.forward(mul216_out1);
        let add133_out1 = add130_out1.add(linear76_out1);
        let layernormalization39_out1 = {
            let dtype = add133_out1.clone().dtype();
            self.layernormalization39
                .forward(add133_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear77_out1 = self.linear77.forward(layernormalization39_out1.clone());
        let shape104_out1: [i64; 3] = {
            let axes = &layernormalization39_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather83_out1 = shape104_out1[0] as i64;
        let unsqueeze131_out1 = [gather83_out1 as i64];
        let constant1080_out1: [i64; 1] = [64i64];
        let constant1077_out1: [i64; 1] = [-1i64];
        let constant1078_out1: [i64; 1] = [3i64];
        let constant1079_out1: [i64; 1] = [12i64];
        let concat82_out1: [i64; 5usize] = [
            &unsqueeze131_out1[..],
            &constant1077_out1[..],
            &constant1078_out1[..],
            &constant1079_out1[..],
            &constant1080_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape42_out1 = linear77_out1.reshape(concat82_out1);
        let transpose61_out1 = reshape42_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose61_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split20_out1, split20_out2, split20_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze58_out1 = split20_out1.squeeze_dims::<4>(&[2]);
        let squeeze59_out1 = split20_out2.squeeze_dims::<4>(&[2]);
        let squeeze60_out1 = split20_out3.squeeze_dims::<4>(&[2]);
        let mul217_out1 = squeeze58_out1.clone().mul(unsqueeze22_out1.clone());
        let shape105_out1: [i64; 4] = {
            let axes = &squeeze58_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather84_out1 = shape105_out1[3] as i64;
        let constant1086_out1 = 2i64;
        let div96_out1 = gather84_out1 / constant1086_out1;
        let unsqueeze132_out1 = [div96_out1 as i64];
        let slice134_out1 = squeeze58_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze132_out1[0]]);
        let unsqueeze133_out1 = [div96_out1 as i64];
        let slice135_out1 = squeeze58_out1
            .slice(s![.., .., .., unsqueeze133_out1[0]..9223372036854775807]);
        let neg39_out1 = slice135_out1.neg();
        let concat83_out1 = burn::tensor::Tensor::cat(
            [neg39_out1, slice134_out1].into(),
            3,
        );
        let mul218_out1 = concat83_out1.mul(unsqueeze23_out1.clone());
        let add134_out1 = mul217_out1.add(mul218_out1);
        let mul219_out1 = squeeze59_out1.clone().mul(unsqueeze22_out1.clone());
        let shape106_out1: [i64; 4] = {
            let axes = &squeeze59_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather85_out1 = shape106_out1[3] as i64;
        let constant1096_out1 = 2i64;
        let div97_out1 = gather85_out1 / constant1096_out1;
        let unsqueeze134_out1 = [div97_out1 as i64];
        let slice136_out1 = squeeze59_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze134_out1[0]]);
        let unsqueeze135_out1 = [div97_out1 as i64];
        let slice137_out1 = squeeze59_out1
            .slice(s![.., .., .., unsqueeze135_out1[0]..9223372036854775807]);
        let neg40_out1 = slice137_out1.neg();
        let concat84_out1 = burn::tensor::Tensor::cat(
            [neg40_out1, slice136_out1].into(),
            3,
        );
        let mul220_out1 = concat84_out1.mul(unsqueeze23_out1.clone());
        let add135_out1 = mul219_out1.add(mul220_out1);
        let shape107_out1: [i64; 4] = {
            let axes = &add134_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice138_out1: [i64; 1] = shape107_out1[3..4].try_into().unwrap();
        let cast137_out1 = {
            let shape_array = slice138_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt58_out1 = cast137_out1.sqrt();
        let constant1107_out1 = self.constant1107.val();
        let div98_out1 = constant1107_out1.div(sqrt58_out1);
        let transpose62_out1 = add135_out1.permute([0, 1, 3, 2]);
        let sqrt59_out1 = div98_out1.sqrt();
        let mul221_out1 = add134_out1
            .mul((sqrt59_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul222_out1 = transpose62_out1
            .mul((sqrt59_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul118_out1 = mul221_out1.matmul(mul222_out1);
        let add136_out1 = matmul118_out1.add(where3_out1.clone());
        let softmax20_out1 = burn::tensor::activation::softmax(add136_out1, 3);
        let matmul119_out1 = softmax20_out1.matmul(squeeze60_out1);
        let transpose63_out1 = matmul119_out1.permute([0, 2, 1, 3]);
        let unsqueeze136_out1 = [gather83_out1 as i64];
        let constant1110_out1: [i64; 1] = [768i64];
        let constant1109_out1: [i64; 1] = [-1i64];
        let concat85_out1: [i64; 3usize] = [
            &unsqueeze136_out1[..],
            &constant1109_out1[..],
            &constant1110_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape43_out1 = transpose63_out1.reshape(concat85_out1);
        let linear78_out1 = self.linear78.forward(reshape43_out1);
        let add137_out1 = add133_out1.add(linear78_out1);
        let layernormalization40_out1 = {
            let dtype = add137_out1.clone().dtype();
            self.layernormalization40
                .forward(add137_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear79_out1 = self.linear79.forward(layernormalization40_out1);
        let shape108_out1: [i64; 3] = {
            let axes = &linear79_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant1112_out1: [i64; 1] = [-1i64];
        let gather86_out1: [i64; 1usize] = constant1112_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape108_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape108_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant1114_out1: [i64; 1] = [1i64];
        let add138_out1 = {
            let __lhs = gather86_out1;
            let __rhs = constant1114_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant1115_out1: [i64; 1] = [2i64];
        let div99_out1 = {
            let __lhs = add138_out1;
            let __rhs = constant1115_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice139_out1 = linear79_out1.clone().slice(s![.., .., 0..div99_out1[0]]);
        let constant1117_out1: [i64; 1] = [2i64];
        let mul224_out1 = {
            let __lhs = div99_out1;
            let __rhs = constant1117_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice140_out1 = linear79_out1
            .slice(s![.., .., div99_out1[0]..mul224_out1[0]]);
        let constant1118_out1 = self.constant1118.val();
        let div100_out1 = slice139_out1
            .clone()
            .div((constant1118_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf20_out1 = div100_out1.erf();
        let constant1119_out1 = self.constant1119.val();
        let add139_out1 = erf20_out1
            .add((constant1119_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul225_out1 = slice139_out1.mul(add139_out1);
        let constant1120_out1 = self.constant1120.val();
        let mul226_out1 = mul225_out1
            .mul((constant1120_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul227_out1 = mul226_out1.mul(slice140_out1);
        let linear80_out1 = self.linear80.forward(mul227_out1);
        let add140_out1 = add137_out1.add(linear80_out1);
        let layernormalization41_out1 = {
            let dtype = add140_out1.clone().dtype();
            self.layernormalization41
                .forward(add140_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear81_out1 = self.linear81.forward(layernormalization41_out1.clone());
        let shape109_out1: [i64; 3] = {
            let axes = &layernormalization41_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather87_out1 = shape109_out1[0] as i64;
        let unsqueeze137_out1 = [gather87_out1 as i64];
        let constant1127_out1: [i64; 1] = [64i64];
        let constant1124_out1: [i64; 1] = [-1i64];
        let constant1125_out1: [i64; 1] = [3i64];
        let constant1126_out1: [i64; 1] = [12i64];
        let concat86_out1: [i64; 5usize] = [
            &unsqueeze137_out1[..],
            &constant1124_out1[..],
            &constant1125_out1[..],
            &constant1126_out1[..],
            &constant1127_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape44_out1 = linear81_out1.reshape(concat86_out1);
        let transpose64_out1 = reshape44_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose64_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split21_out1, split21_out2, split21_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze61_out1 = split21_out1.squeeze_dims::<4>(&[2]);
        let squeeze62_out1 = split21_out2.squeeze_dims::<4>(&[2]);
        let squeeze63_out1 = split21_out3.squeeze_dims::<4>(&[2]);
        let mul228_out1 = squeeze61_out1.clone().mul(unsqueeze22_out1.clone());
        let shape110_out1: [i64; 4] = {
            let axes = &squeeze61_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather88_out1 = shape110_out1[3] as i64;
        let constant1133_out1 = 2i64;
        let div101_out1 = gather88_out1 / constant1133_out1;
        let unsqueeze138_out1 = [div101_out1 as i64];
        let slice141_out1 = squeeze61_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze138_out1[0]]);
        let unsqueeze139_out1 = [div101_out1 as i64];
        let slice142_out1 = squeeze61_out1
            .slice(s![.., .., .., unsqueeze139_out1[0]..9223372036854775807]);
        let neg41_out1 = slice142_out1.neg();
        let concat87_out1 = burn::tensor::Tensor::cat(
            [neg41_out1, slice141_out1].into(),
            3,
        );
        let mul229_out1 = concat87_out1.mul(unsqueeze23_out1.clone());
        let add141_out1 = mul228_out1.add(mul229_out1);
        let mul230_out1 = squeeze62_out1.clone().mul(unsqueeze22_out1);
        let shape111_out1: [i64; 4] = {
            let axes = &squeeze62_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather89_out1 = shape111_out1[3] as i64;
        let constant1143_out1 = 2i64;
        let div102_out1 = gather89_out1 / constant1143_out1;
        let unsqueeze140_out1 = [div102_out1 as i64];
        let slice143_out1 = squeeze62_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze140_out1[0]]);
        let unsqueeze141_out1 = [div102_out1 as i64];
        let slice144_out1 = squeeze62_out1
            .slice(s![.., .., .., unsqueeze141_out1[0]..9223372036854775807]);
        let neg42_out1 = slice144_out1.neg();
        let concat88_out1 = burn::tensor::Tensor::cat(
            [neg42_out1, slice143_out1].into(),
            3,
        );
        let mul231_out1 = concat88_out1.mul(unsqueeze23_out1);
        let add142_out1 = mul230_out1.add(mul231_out1);
        let shape112_out1: [i64; 4] = {
            let axes = &add141_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice145_out1: [i64; 1] = shape112_out1[3..4].try_into().unwrap();
        let cast143_out1 = {
            let shape_array = slice145_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt61_out1 = cast143_out1.sqrt();
        let constant1154_out1 = self.constant1154.val();
        let div103_out1 = constant1154_out1.div(sqrt61_out1);
        let transpose65_out1 = add142_out1.permute([0, 1, 3, 2]);
        let sqrt62_out1 = div103_out1.sqrt();
        let mul232_out1 = add141_out1
            .mul((sqrt62_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul233_out1 = transpose65_out1
            .mul((sqrt62_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul124_out1 = mul232_out1.matmul(mul233_out1);
        let add143_out1 = matmul124_out1.add(where3_out1);
        let softmax21_out1 = burn::tensor::activation::softmax(add143_out1, 3);
        let matmul125_out1 = softmax21_out1.matmul(squeeze63_out1);
        (matmul125_out1, gather87_out1, add140_out1)
    }
}
#[derive(Module, Debug)]
pub struct Submodule20 {
    linear82: Linear,
    layernormalization42: LayerNorm,
    linear83: Linear,
    constant1165: burn::module::Param<Tensor<1>>,
    constant1166: burn::module::Param<Tensor<1>>,
    constant1167: burn::module::Param<Tensor<1>>,
    linear84: Linear,
    layernormalization43: LayerNorm,
    linear85: Linear,
    constant1201: burn::module::Param<Tensor<1>>,
    linear86: Linear,
    layernormalization44: LayerNorm,
    linear87: Linear,
    constant1212: burn::module::Param<Tensor<1>>,
    constant1213: burn::module::Param<Tensor<1>>,
    constant1214: burn::module::Param<Tensor<1>>,
    linear88: Linear,
    layernormalization45: LayerNorm,
    #[module(skip)]
    device: Device,
}
impl Submodule20 {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let linear82 = LinearConfig::new(768, 768).with_bias(false).init(device);
        let layernormalization42 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear83 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1165: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1166: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1167: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear84 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization43 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear85 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1201: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::zeros([1], (device, burn::tensor::DType::F32)),
            device.clone(),
            false,
            [1].into(),
        );
        let linear86 = LinearConfig::new(768, 768).with_bias(false).init(device);
        let layernormalization44 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        let linear87 = LinearConfig::new(768, 2304).with_bias(false).init(device);
        let constant1212: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1.4142135381698608f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1213: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([1f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let constant1214: burn::module::Param<Tensor<1>> = burn::module::Param::uninitialized(
            burn::module::ParamId::new(),
            move |device, _require_grad| Tensor::<
                1,
            >::from_data(
                burn::tensor::TensorData::from([0.5f64]),
                (device, burn::tensor::DType::F32),
            ),
            device.clone(),
            false,
            [1].into(),
        );
        let linear88 = LinearConfig::new(1152, 768).with_bias(false).init(device);
        let layernormalization45 = LayerNormConfig::new(768)
            .with_epsilon(0.0000000000009999999960041972f64)
            .with_bias(true)
            .init(device);
        Self {
            linear82,
            layernormalization42,
            linear83,
            constant1165,
            constant1166,
            constant1167,
            linear84,
            layernormalization43,
            linear85,
            constant1201,
            linear86,
            layernormalization44,
            linear87,
            constant1212,
            constant1213,
            constant1214,
            linear88,
            layernormalization45,
            device: device.clone(),
        }
    }
    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        matmul125_out1: Tensor<4>,
        gather87_out1: i64,
        add140_out1: Tensor<3>,
        unsqueeze13_out1: Tensor<4>,
        unsqueeze14_out1: Tensor<4>,
        where2_out1: Tensor<4>,
    ) -> Tensor<3> {
        let transpose66_out1 = matmul125_out1.permute([0, 2, 1, 3]);
        let unsqueeze142_out1 = [gather87_out1 as i64];
        let constant1157_out1: [i64; 1] = [768i64];
        let constant1156_out1: [i64; 1] = [-1i64];
        let concat89_out1: [i64; 3usize] = [
            &unsqueeze142_out1[..],
            &constant1156_out1[..],
            &constant1157_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape45_out1 = transpose66_out1.reshape(concat89_out1);
        let linear82_out1 = self.linear82.forward(reshape45_out1);
        let add144_out1 = add140_out1.add(linear82_out1);
        let layernormalization42_out1 = {
            let dtype = add144_out1.clone().dtype();
            self.layernormalization42
                .forward(add144_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear83_out1 = self.linear83.forward(layernormalization42_out1);
        let shape113_out1: [i64; 3] = {
            let axes = &linear83_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant1159_out1: [i64; 1] = [-1i64];
        let gather90_out1: [i64; 1usize] = constant1159_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape113_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape113_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant1161_out1: [i64; 1] = [1i64];
        let add145_out1 = {
            let __lhs = gather90_out1;
            let __rhs = constant1161_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant1162_out1: [i64; 1] = [2i64];
        let div104_out1 = {
            let __lhs = add145_out1;
            let __rhs = constant1162_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice146_out1 = linear83_out1.clone().slice(s![.., .., 0..div104_out1[0]]);
        let constant1164_out1: [i64; 1] = [2i64];
        let mul235_out1 = {
            let __lhs = div104_out1;
            let __rhs = constant1164_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice147_out1 = linear83_out1
            .slice(s![.., .., div104_out1[0]..mul235_out1[0]]);
        let constant1165_out1 = self.constant1165.val();
        let div105_out1 = slice146_out1
            .clone()
            .div((constant1165_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf21_out1 = div105_out1.erf();
        let constant1166_out1 = self.constant1166.val();
        let add146_out1 = erf21_out1
            .add((constant1166_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul236_out1 = slice146_out1.mul(add146_out1);
        let constant1167_out1 = self.constant1167.val();
        let mul237_out1 = mul236_out1
            .mul((constant1167_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul238_out1 = mul237_out1.mul(slice147_out1);
        let linear84_out1 = self.linear84.forward(mul238_out1);
        let add147_out1 = add144_out1.add(linear84_out1);
        let layernormalization43_out1 = {
            let dtype = add147_out1.clone().dtype();
            self.layernormalization43
                .forward(add147_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear85_out1 = self.linear85.forward(layernormalization43_out1.clone());
        let shape114_out1: [i64; 3] = {
            let axes = &layernormalization43_out1.dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather91_out1 = shape114_out1[0] as i64;
        let unsqueeze143_out1 = [gather91_out1 as i64];
        let constant1174_out1: [i64; 1] = [64i64];
        let constant1171_out1: [i64; 1] = [-1i64];
        let constant1172_out1: [i64; 1] = [3i64];
        let constant1173_out1: [i64; 1] = [12i64];
        let concat90_out1: [i64; 5usize] = [
            &unsqueeze143_out1[..],
            &constant1171_out1[..],
            &constant1172_out1[..],
            &constant1173_out1[..],
            &constant1174_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape46_out1 = linear85_out1.reshape(concat90_out1);
        let transpose67_out1 = reshape46_out1.permute([0, 3, 2, 1, 4]);
        let split_tensors = transpose67_out1.split_with_sizes([1, 1, 1].into(), 2);
        let [split22_out1, split22_out2, split22_out3] = split_tensors
            .try_into()
            .unwrap();
        let squeeze64_out1 = split22_out1.squeeze_dims::<4>(&[2]);
        let squeeze65_out1 = split22_out2.squeeze_dims::<4>(&[2]);
        let squeeze66_out1 = split22_out3.squeeze_dims::<4>(&[2]);
        let mul239_out1 = squeeze64_out1.clone().mul(unsqueeze13_out1.clone());
        let shape115_out1: [i64; 4] = {
            let axes = &squeeze64_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather92_out1 = shape115_out1[3] as i64;
        let constant1180_out1 = 2i64;
        let div106_out1 = gather92_out1 / constant1180_out1;
        let unsqueeze144_out1 = [div106_out1 as i64];
        let slice148_out1 = squeeze64_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze144_out1[0]]);
        let unsqueeze145_out1 = [div106_out1 as i64];
        let slice149_out1 = squeeze64_out1
            .slice(s![.., .., .., unsqueeze145_out1[0]..9223372036854775807]);
        let neg43_out1 = slice149_out1.neg();
        let concat91_out1 = burn::tensor::Tensor::cat(
            [neg43_out1, slice148_out1].into(),
            3,
        );
        let mul240_out1 = concat91_out1.mul(unsqueeze14_out1.clone());
        let add148_out1 = mul239_out1.add(mul240_out1);
        let mul241_out1 = squeeze65_out1.clone().mul(unsqueeze13_out1);
        let shape116_out1: [i64; 4] = {
            let axes = &squeeze65_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let gather93_out1 = shape116_out1[3] as i64;
        let constant1190_out1 = 2i64;
        let div107_out1 = gather93_out1 / constant1190_out1;
        let unsqueeze146_out1 = [div107_out1 as i64];
        let slice150_out1 = squeeze65_out1
            .clone()
            .slice(s![.., .., .., 0..unsqueeze146_out1[0]]);
        let unsqueeze147_out1 = [div107_out1 as i64];
        let slice151_out1 = squeeze65_out1
            .slice(s![.., .., .., unsqueeze147_out1[0]..9223372036854775807]);
        let neg44_out1 = slice151_out1.neg();
        let concat92_out1 = burn::tensor::Tensor::cat(
            [neg44_out1, slice150_out1].into(),
            3,
        );
        let mul242_out1 = concat92_out1.mul(unsqueeze14_out1);
        let add149_out1 = mul241_out1.add(mul242_out1);
        let shape117_out1: [i64; 4] = {
            let axes = &add148_out1.clone().dims()[0..4];
            let mut output = [0i64; 4];
            for i in 0..4 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let slice152_out1: [i64; 1] = shape117_out1[3..4].try_into().unwrap();
        let cast149_out1 = {
            let shape_array = slice152_out1 as [i64; 1usize];
            let float_array: [f64; 1usize] = shape_array.map(|x| x as f64);
            Tensor::<
                1,
            >::from_data(
                TensorData::from(float_array),
                (&self.device, burn::tensor::DType::F32),
            )
        };
        let sqrt64_out1 = cast149_out1.sqrt();
        let constant1201_out1 = self.constant1201.val();
        let div108_out1 = constant1201_out1.div(sqrt64_out1);
        let transpose68_out1 = add149_out1.permute([0, 1, 3, 2]);
        let sqrt65_out1 = div108_out1.sqrt();
        let mul243_out1 = add148_out1
            .mul((sqrt65_out1.clone()).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let mul244_out1 = transpose68_out1
            .mul((sqrt65_out1).unsqueeze_dims(&[0isize, 1isize, 2isize]));
        let matmul130_out1 = mul243_out1.matmul(mul244_out1);
        let add150_out1 = matmul130_out1.add(where2_out1);
        let softmax22_out1 = burn::tensor::activation::softmax(add150_out1, 3);
        let matmul131_out1 = softmax22_out1.matmul(squeeze66_out1);
        let transpose69_out1 = matmul131_out1.permute([0, 2, 1, 3]);
        let unsqueeze148_out1 = [gather91_out1 as i64];
        let constant1204_out1: [i64; 1] = [768i64];
        let constant1203_out1: [i64; 1] = [-1i64];
        let concat93_out1: [i64; 3usize] = [
            &unsqueeze148_out1[..],
            &constant1203_out1[..],
            &constant1204_out1[..],
        ]
            .concat()
            .try_into()
            .unwrap();
        let reshape47_out1 = transpose69_out1.reshape(concat93_out1);
        let linear86_out1 = self.linear86.forward(reshape47_out1);
        let add151_out1 = add147_out1.add(linear86_out1);
        let layernormalization44_out1 = {
            let dtype = add151_out1.clone().dtype();
            self.layernormalization44
                .forward(add151_out1.clone().cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        let linear87_out1 = self.linear87.forward(layernormalization44_out1);
        let shape118_out1: [i64; 3] = {
            let axes = &linear87_out1.clone().dims()[0..3];
            let mut output = [0i64; 3];
            for i in 0..3 {
                output[i] = axes[i] as i64;
            }
            output
        };
        let constant1206_out1: [i64; 1] = [-1i64];
        let gather94_out1: [i64; 1usize] = constant1206_out1
            .iter()
            .map(|&idx| {
                let actual_idx = if idx < 0 {
                    (shape118_out1.len() as i64 + idx) as usize
                } else {
                    idx as usize
                };
                shape118_out1[actual_idx]
            })
            .collect::<alloc::vec::Vec<_>>()
            .try_into()
            .unwrap();
        let constant1208_out1: [i64; 1] = [1i64];
        let add152_out1 = {
            let __lhs = gather94_out1;
            let __rhs = constant1208_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_add(__rhs[__i]))
        };
        let constant1209_out1: [i64; 1] = [2i64];
        let div109_out1 = {
            let __lhs = add152_out1;
            let __rhs = constant1209_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| if __rhs[__i] != 0 { __lhs[__i] / __rhs[__i] } else { __lhs[__i] })
        };
        let slice153_out1 = linear87_out1.clone().slice(s![.., .., 0..div109_out1[0]]);
        let constant1211_out1: [i64; 1] = [2i64];
        let mul246_out1 = {
            let __lhs = div109_out1;
            let __rhs = constant1211_out1;
            core::array::from_fn::<
                i64,
                1usize,
                _,
            >(|__i| (__lhs[__i]).saturating_mul(__rhs[__i]))
        };
        let slice154_out1 = linear87_out1
            .slice(s![.., .., div109_out1[0]..mul246_out1[0]]);
        let constant1212_out1 = self.constant1212.val();
        let div110_out1 = slice153_out1
            .clone()
            .div((constant1212_out1).unsqueeze_dims(&[0isize, 1isize]));
        let erf22_out1 = div110_out1.erf();
        let constant1213_out1 = self.constant1213.val();
        let add153_out1 = erf22_out1
            .add((constant1213_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul247_out1 = slice153_out1.mul(add153_out1);
        let constant1214_out1 = self.constant1214.val();
        let mul248_out1 = mul247_out1
            .mul((constant1214_out1).unsqueeze_dims(&[0isize, 1isize]));
        let mul249_out1 = mul248_out1.mul(slice154_out1);
        let linear88_out1 = self.linear88.forward(mul249_out1);
        let add154_out1 = add151_out1.add(linear88_out1);
        let layernormalization45_out1 = {
            let dtype = add154_out1.dtype();
            self.layernormalization45
                .forward(add154_out1.cast(burn::tensor::DType::F32))
                .cast(dtype)
        };
        layernormalization45_out1
    }
}

#[derive(Module, Debug)]
pub struct Model {
    submodule1: Submodule1,
    submodule2: Submodule2,
    submodule3: Submodule3,
    submodule4: Submodule4,
    submodule5: Submodule5,
    submodule6: Submodule6,
    submodule7: Submodule7,
    submodule8: Submodule8,
    submodule9: Submodule9,
    submodule10: Submodule10,
    submodule11: Submodule11,
    submodule12: Submodule12,
    submodule13: Submodule13,
    submodule14: Submodule14,
    submodule15: Submodule15,
    submodule16: Submodule16,
    submodule17: Submodule17,
    submodule18: Submodule18,
    submodule19: Submodule19,
    submodule20: Submodule20,
    #[module(skip)]
    device: Device,
}


extern crate std;

impl Default for Model {
    fn default() -> Self {
        Self::from_file("generated/model.bpk", &Default::default())
    }
}

impl Model {
    /// Load model weights from a burnpack file.
    pub fn from_file<P: AsRef<std::path::Path>>(file: P, device: &Device) -> Self {
        let mut model = Self::new(device);
        let mut store = BurnpackStore::from_file(&file);
        model
            .load_from(&mut store)
            .unwrap_or_else(|e| {
                panic!("Failed to load burnpack file {}: {e}", file.as_ref().display())
            });
        model
    }

    /// Load model weights from in-memory bytes.
    ///
    /// The bytes must be the contents of a `.bpk` file.
    pub fn from_bytes(bytes: Bytes, device: &Device) -> Self {
        let mut model = Self::new(device);
        let mut store = BurnpackStore::from_bytes(Some(bytes));
        model
            .load_from(&mut store)
            .unwrap_or_else(|e| panic!("Failed to load burnpack bytes: {e}"));
        model
    }
}

impl Model {
    #[allow(unused_variables)]
    pub fn new(device: &Device) -> Self {
        let submodule1 = Submodule1::new(device);
        let submodule2 = Submodule2::new(device);
        let submodule3 = Submodule3::new(device);
        let submodule4 = Submodule4::new(device);
        let submodule5 = Submodule5::new(device);
        let submodule6 = Submodule6::new(device);
        let submodule7 = Submodule7::new(device);
        let submodule8 = Submodule8::new(device);
        let submodule9 = Submodule9::new(device);
        let submodule10 = Submodule10::new(device);
        let submodule11 = Submodule11::new(device);
        let submodule12 = Submodule12::new(device);
        let submodule13 = Submodule13::new(device);
        let submodule14 = Submodule14::new(device);
        let submodule15 = Submodule15::new(device);
        let submodule16 = Submodule16::new(device);
        let submodule17 = Submodule17::new(device);
        let submodule18 = Submodule18::new(device);
        let submodule19 = Submodule19::new(device);
        let submodule20 = Submodule20::new(device);
        Self {
            submodule1,
            submodule2,
            submodule3,
            submodule4,
            submodule5,
            submodule6,
            submodule7,
            submodule8,
            submodule9,
            submodule10,
            submodule11,
            submodule12,
            submodule13,
            submodule14,
            submodule15,
            submodule16,
            submodule17,
            submodule18,
            submodule19,
            submodule20,
            device: device.clone(),
        }
    }

    #[allow(clippy::let_and_return, clippy::approx_constant)]
    pub fn forward(
        &self,
        input_ids: Tensor<2, Int>,
        attention_mask: Tensor<2, Int>,
    ) -> Tensor<3> {
        let (
            add4_out1,
            gather7_out1,
            cast10_out1,
            where3_out1,
            unsqueeze13_out1,
            unsqueeze14_out1,
            where2_out1,
        ) = self.submodule1.forward(input_ids, attention_mask);
        let (add11_out1, unsqueeze22_out1, unsqueeze23_out1) = self
            .submodule2
            .forward(add4_out1, gather7_out1, cast10_out1, where3_out1.clone());
        let add18_out1 = self
            .submodule3
            .forward(
                add11_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add25_out1 = self
            .submodule4
            .forward(
                add18_out1,
                unsqueeze13_out1.clone(),
                unsqueeze14_out1.clone(),
                where2_out1.clone(),
            );
        let add32_out1 = self
            .submodule5
            .forward(
                add25_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add39_out1 = self
            .submodule6
            .forward(
                add32_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add46_out1 = self
            .submodule7
            .forward(
                add39_out1,
                unsqueeze13_out1.clone(),
                unsqueeze14_out1.clone(),
                where2_out1.clone(),
            );
        let add53_out1 = self
            .submodule8
            .forward(
                add46_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add60_out1 = self
            .submodule9
            .forward(
                add53_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add67_out1 = self
            .submodule10
            .forward(
                add60_out1,
                unsqueeze13_out1.clone(),
                unsqueeze14_out1.clone(),
                where2_out1.clone(),
            );
        let add74_out1 = self
            .submodule11
            .forward(
                add67_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add81_out1 = self
            .submodule12
            .forward(
                add74_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add88_out1 = self
            .submodule13
            .forward(
                add81_out1,
                unsqueeze13_out1.clone(),
                unsqueeze14_out1.clone(),
                where2_out1.clone(),
            );
        let add95_out1 = self
            .submodule14
            .forward(
                add88_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add102_out1 = self
            .submodule15
            .forward(
                add95_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add109_out1 = self
            .submodule16
            .forward(
                add102_out1,
                unsqueeze13_out1.clone(),
                unsqueeze14_out1.clone(),
                where2_out1.clone(),
            );
        let add116_out1 = self
            .submodule17
            .forward(
                add109_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let add123_out1 = self
            .submodule18
            .forward(
                add116_out1,
                unsqueeze22_out1.clone(),
                unsqueeze23_out1.clone(),
                where3_out1.clone(),
            );
        let (matmul125_out1, gather87_out1, add140_out1) = self
            .submodule19
            .forward(
                add123_out1,
                unsqueeze13_out1.clone(),
                unsqueeze14_out1.clone(),
                where2_out1.clone(),
                unsqueeze22_out1,
                unsqueeze23_out1,
                where3_out1,
            );
        let layernormalization45_out1 = self
            .submodule20
            .forward(
                matmul125_out1,
                gather87_out1,
                add140_out1,
                unsqueeze13_out1,
                unsqueeze14_out1,
                where2_out1,
            );
        layernormalization45_out1
    }
}
