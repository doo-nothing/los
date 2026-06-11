/* ------------------------------------------------------------
author: "doo-nothing / AU Supply"
license: "AGPL-3.0-or-later"
name: "tap8fx"
Code generated with Faust 2.85.5 (https://faust.grame.fr)
Compilation options: -lang rust -fpga-mem-th 4 -ct 1 -cn Tap8Fx -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0
------------------------------------------------------------ */

#[repr(C)]
pub struct Tap8Fx {
	IOTA0: i32,
	fVec0: [F32;131072],
	fRec0: [F32;2],
	fRec10: [F32;2],
	fVec1: [F32;8192],
	fSampleRate: i32,
	fConst0: F32,
	iConst1: i32,
	fRec9: [F32;2],
	fRec12: [F32;2],
	fVec2: [F32;8192],
	iConst2: i32,
	fRec11: [F32;2],
	fRec14: [F32;2],
	fVec3: [F32;8192],
	iConst3: i32,
	fRec13: [F32;2],
	fRec16: [F32;2],
	fVec4: [F32;8192],
	iConst4: i32,
	fRec15: [F32;2],
	fRec18: [F32;2],
	fVec5: [F32;8192],
	iConst5: i32,
	fRec17: [F32;2],
	fRec20: [F32;2],
	fVec6: [F32;8192],
	iConst6: i32,
	fRec19: [F32;2],
	fRec22: [F32;2],
	fVec7: [F32;8192],
	iConst7: i32,
	fRec21: [F32;2],
	fRec24: [F32;2],
	fVec8: [F32;8192],
	iConst8: i32,
	fRec23: [F32;2],
	fVec9: [F32;2048],
	iConst9: i32,
	fRec7: [F32;2],
	fVec10: [F32;2048],
	iConst10: i32,
	fRec5: [F32;2],
	fVec11: [F32;2048],
	iConst11: i32,
	fRec3: [F32;2],
	fVec12: [F32;1024],
	iConst12: i32,
	fRec1: [F32;2],
}


pub const FAUST_INPUTS: usize = 1;
pub const FAUST_OUTPUTS: usize = 2;
pub const FAUST_ACTIVES: usize = 0;
pub const FAUST_PASSIVES: usize = 0;

impl Tap8Fx {
		
	pub fn new() -> Tap8Fx { 
		Tap8Fx {
			IOTA0: 0,
			fVec0: [0.0;131072],
			fRec0: [0.0;2],
			fRec10: [0.0;2],
			fVec1: [0.0;8192],
			fSampleRate: 0,
			fConst0: 0.0,
			iConst1: 0,
			fRec9: [0.0;2],
			fRec12: [0.0;2],
			fVec2: [0.0;8192],
			iConst2: 0,
			fRec11: [0.0;2],
			fRec14: [0.0;2],
			fVec3: [0.0;8192],
			iConst3: 0,
			fRec13: [0.0;2],
			fRec16: [0.0;2],
			fVec4: [0.0;8192],
			iConst4: 0,
			fRec15: [0.0;2],
			fRec18: [0.0;2],
			fVec5: [0.0;8192],
			iConst5: 0,
			fRec17: [0.0;2],
			fRec20: [0.0;2],
			fVec6: [0.0;8192],
			iConst6: 0,
			fRec19: [0.0;2],
			fRec22: [0.0;2],
			fVec7: [0.0;8192],
			iConst7: 0,
			fRec21: [0.0;2],
			fRec24: [0.0;2],
			fVec8: [0.0;8192],
			iConst8: 0,
			fRec23: [0.0;2],
			fVec9: [0.0;2048],
			iConst9: 0,
			fRec7: [0.0;2],
			fVec10: [0.0;2048],
			iConst10: 0,
			fRec5: [0.0;2],
			fVec11: [0.0;2048],
			iConst11: 0,
			fRec3: [0.0;2],
			fVec12: [0.0;1024],
			iConst12: 0,
			fRec1: [0.0;2],
		}
	}
	pub fn metadata(&self, m: &mut dyn Meta) { 
		m.declare("author", r"doo-nothing / AU Supply");
		m.declare("compile_options", r"-lang rust -fpga-mem-th 4 -ct 1 -cn Tap8Fx -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0");
		m.declare("delays.lib/name", r"Faust Delay Library");
		m.declare("delays.lib/version", r"1.2.0");
		m.declare("filename", r"tap8fx.dsp");
		m.declare("filters.lib/allpass_comb:author", r"Julius O. Smith III");
		m.declare("filters.lib/allpass_comb:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/allpass_comb:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/lowpass0_highpass1", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/name", r"Faust Filters Library");
		m.declare("filters.lib/version", r"1.7.1");
		m.declare("license", r"AGPL-3.0-or-later");
		m.declare("maths.lib/author", r"GRAME");
		m.declare("maths.lib/copyright", r"GRAME");
		m.declare("maths.lib/license", r"LGPL with exception");
		m.declare("maths.lib/name", r"Faust Math Library");
		m.declare("maths.lib/version", r"2.9.0");
		m.declare("misceffects.lib/name", r"Misc Effects Library");
		m.declare("misceffects.lib/version", r"2.5.2");
		m.declare("name", r"tap8fx");
		m.declare("platform.lib/name", r"Generic Platform Library");
		m.declare("platform.lib/version", r"1.3.0");
		m.declare("reverbs.lib/mono_freeverb:author", r"Romain Michon");
		m.declare("reverbs.lib/name", r"Faust Reverb Library");
		m.declare("reverbs.lib/version", r"1.5.1");
	}

	pub fn get_sample_rate(&self) -> i32 { self.fSampleRate as i32}
	
	pub fn class_init(sample_rate: i32) {
		// Obtaining locks on 0 static var(s)
	}
	pub fn instance_reset_params(&mut self) {
	}
	pub fn instance_clear(&mut self) {
		self.IOTA0 = 0;
		for l0 in 0..131072 {
			self.fVec0[l0 as usize] = 0.0;
		}
		for l1 in 0..2 {
			self.fRec0[l1 as usize] = 0.0;
		}
		for l2 in 0..2 {
			self.fRec10[l2 as usize] = 0.0;
		}
		for l3 in 0..8192 {
			self.fVec1[l3 as usize] = 0.0;
		}
		for l4 in 0..2 {
			self.fRec9[l4 as usize] = 0.0;
		}
		for l5 in 0..2 {
			self.fRec12[l5 as usize] = 0.0;
		}
		for l6 in 0..8192 {
			self.fVec2[l6 as usize] = 0.0;
		}
		for l7 in 0..2 {
			self.fRec11[l7 as usize] = 0.0;
		}
		for l8 in 0..2 {
			self.fRec14[l8 as usize] = 0.0;
		}
		for l9 in 0..8192 {
			self.fVec3[l9 as usize] = 0.0;
		}
		for l10 in 0..2 {
			self.fRec13[l10 as usize] = 0.0;
		}
		for l11 in 0..2 {
			self.fRec16[l11 as usize] = 0.0;
		}
		for l12 in 0..8192 {
			self.fVec4[l12 as usize] = 0.0;
		}
		for l13 in 0..2 {
			self.fRec15[l13 as usize] = 0.0;
		}
		for l14 in 0..2 {
			self.fRec18[l14 as usize] = 0.0;
		}
		for l15 in 0..8192 {
			self.fVec5[l15 as usize] = 0.0;
		}
		for l16 in 0..2 {
			self.fRec17[l16 as usize] = 0.0;
		}
		for l17 in 0..2 {
			self.fRec20[l17 as usize] = 0.0;
		}
		for l18 in 0..8192 {
			self.fVec6[l18 as usize] = 0.0;
		}
		for l19 in 0..2 {
			self.fRec19[l19 as usize] = 0.0;
		}
		for l20 in 0..2 {
			self.fRec22[l20 as usize] = 0.0;
		}
		for l21 in 0..8192 {
			self.fVec7[l21 as usize] = 0.0;
		}
		for l22 in 0..2 {
			self.fRec21[l22 as usize] = 0.0;
		}
		for l23 in 0..2 {
			self.fRec24[l23 as usize] = 0.0;
		}
		for l24 in 0..8192 {
			self.fVec8[l24 as usize] = 0.0;
		}
		for l25 in 0..2 {
			self.fRec23[l25 as usize] = 0.0;
		}
		for l26 in 0..2048 {
			self.fVec9[l26 as usize] = 0.0;
		}
		for l27 in 0..2 {
			self.fRec7[l27 as usize] = 0.0;
		}
		for l28 in 0..2048 {
			self.fVec10[l28 as usize] = 0.0;
		}
		for l29 in 0..2 {
			self.fRec5[l29 as usize] = 0.0;
		}
		for l30 in 0..2048 {
			self.fVec11[l30 as usize] = 0.0;
		}
		for l31 in 0..2 {
			self.fRec3[l31 as usize] = 0.0;
		}
		for l32 in 0..1024 {
			self.fVec12[l32 as usize] = 0.0;
		}
		for l33 in 0..2 {
			self.fRec1[l33 as usize] = 0.0;
		}
	}
	pub fn instance_constants(&mut self, sample_rate: i32) {
		// Obtaining locks on 0 static var(s)
		self.fSampleRate = sample_rate;
		self.fConst0 = F32::min(1.92e+05, F32::max(1.0, (self.fSampleRate) as F32));
		self.iConst1 = core::cmp::max(0, i32::wrapping_add((0.036666665 * self.fConst0) as i32, -1));
		self.iConst2 = core::cmp::max(0, i32::wrapping_add((0.035306122 * self.fConst0) as i32, -1));
		self.iConst3 = core::cmp::max(0, i32::wrapping_add((0.033809524 * self.fConst0) as i32, -1));
		self.iConst4 = core::cmp::max(0, i32::wrapping_add((0.0322449 * self.fConst0) as i32, -1));
		self.iConst5 = core::cmp::max(0, i32::wrapping_add((0.030748298 * self.fConst0) as i32, -1));
		self.iConst6 = core::cmp::max(0, i32::wrapping_add((0.028956916 * self.fConst0) as i32, -1));
		self.iConst7 = core::cmp::max(0, i32::wrapping_add((0.026938776 * self.fConst0) as i32, -1));
		self.iConst8 = core::cmp::max(0, i32::wrapping_add((0.025306122 * self.fConst0) as i32, -1));
		self.iConst9 = core::cmp::min(1024, core::cmp::max(0, i32::wrapping_add((0.0126077095 * self.fConst0) as i32, -1)));
		self.iConst10 = core::cmp::min(1024, core::cmp::max(0, i32::wrapping_add((0.01 * self.fConst0) as i32, -1)));
		self.iConst11 = core::cmp::min(1024, core::cmp::max(0, i32::wrapping_add((0.0077324263 * self.fConst0) as i32, -1)));
		self.iConst12 = core::cmp::min(1024, core::cmp::max(0, i32::wrapping_add((0.0051020407 * self.fConst0) as i32, -1)));
	}
	pub fn instance_init(&mut self, sample_rate: i32) {
		self.instance_constants(sample_rate);
		self.instance_reset_params();
		self.instance_clear();
	}
	pub fn init(&mut self, sample_rate: i32) {
		Tap8Fx::class_init(sample_rate);
		self.instance_init(sample_rate);
	}
	
	pub fn build_user_interface(&self, ui_interface: &mut dyn UI<FaustFloat>) {
		Self::build_user_interface_static(ui_interface);
	}
	
	pub fn build_user_interface_static(ui_interface: &mut dyn UI<FaustFloat>) {
		ui_interface.open_vertical_box("tap8fx");
		ui_interface.close_box();
	}
	
	pub fn get_param(&self, param: ParamIndex) -> Option<FaustFloat> {
		match param.0 {
			_ => None,
		}
	}
	
	pub fn set_param(&mut self, param: ParamIndex, value: FaustFloat) {
		match param.0 {
			_ => {}
		}
	}
	
	pub fn compute(
		&mut self,
		count: usize,
		inputs: &[impl AsRef<[FaustFloat]>],
		outputs: &mut[impl AsMut<[FaustFloat]>],
	) {
		
		// Obtaining locks on 0 static var(s)
		let [inputs0, .. ] = inputs.as_ref() else { panic!("wrong number of input buffers"); };
		let inputs0 = inputs0.as_ref()[..count].iter();
		let [outputs0, outputs1, .. ] = outputs.as_mut() else { panic!("wrong number of output buffers"); };
		let outputs0 = outputs0.as_mut()[..count].iter_mut();
		let outputs1 = outputs1.as_mut()[..count].iter_mut();
		let zipped_iterators = inputs0.zip(outputs0).zip(outputs1);
		for ((input0, output0), output1) in zipped_iterators {
			let mut fTemp0: F32 = (*input0) as F32;
			self.fVec0[(self.IOTA0 & 131071) as usize] = fTemp0;
			self.fRec0[0] = (self.fRec0[1] + 2399.0) % 2.4e+03;
			let mut iTemp1: i32 = (self.fRec0[0]) as i32;
			let mut fTemp2: F32 = F32::floor(self.fRec0[0]);
			let mut fTemp3: F32 = F32::min(0.0016666667 * self.fRec0[0], 1.0);
			let mut fTemp4: F32 = self.fRec0[0] + 2.4e+03;
			let mut iTemp5: i32 = (fTemp4) as i32;
			let mut fTemp6: F32 = F32::floor(fTemp4);
			*output0 = ((self.fVec0[((i32::wrapping_sub(self.IOTA0, core::cmp::min(65537, core::cmp::max(0, iTemp1)))) & 131071) as usize] * (fTemp2 + (1.0 - self.fRec0[0])) + (self.fRec0[0] - fTemp2) * self.fVec0[((i32::wrapping_sub(self.IOTA0, core::cmp::min(65537, core::cmp::max(0, i32::wrapping_add(iTemp1, 1))))) & 131071) as usize]) * fTemp3 + (self.fVec0[((i32::wrapping_sub(self.IOTA0, core::cmp::min(65537, core::cmp::max(0, iTemp5)))) & 131071) as usize] * (fTemp6 + (-2399.0 - self.fRec0[0])) + self.fVec0[((i32::wrapping_sub(self.IOTA0, core::cmp::min(65537, core::cmp::max(0, i32::wrapping_add(iTemp5, 1))))) & 131071) as usize] * (self.fRec0[0] + (2.4e+03 - fTemp6))) * (1.0 - fTemp3)) as FaustFloat;
			self.fRec10[0] = 0.7 * self.fRec10[1] + 0.3 * self.fRec9[1];
			self.fVec1[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec10[0];
			self.fRec9[0] = self.fVec1[((i32::wrapping_sub(self.IOTA0, self.iConst1)) & 8191) as usize];
			self.fRec12[0] = 0.7 * self.fRec12[1] + 0.3 * self.fRec11[1];
			self.fVec2[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec12[0];
			self.fRec11[0] = self.fVec2[((i32::wrapping_sub(self.IOTA0, self.iConst2)) & 8191) as usize];
			self.fRec14[0] = 0.7 * self.fRec14[1] + 0.3 * self.fRec13[1];
			self.fVec3[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec14[0];
			self.fRec13[0] = self.fVec3[((i32::wrapping_sub(self.IOTA0, self.iConst3)) & 8191) as usize];
			self.fRec16[0] = 0.7 * self.fRec16[1] + 0.3 * self.fRec15[1];
			self.fVec4[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec16[0];
			self.fRec15[0] = self.fVec4[((i32::wrapping_sub(self.IOTA0, self.iConst4)) & 8191) as usize];
			self.fRec18[0] = 0.7 * self.fRec18[1] + 0.3 * self.fRec17[1];
			self.fVec5[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec18[0];
			self.fRec17[0] = self.fVec5[((i32::wrapping_sub(self.IOTA0, self.iConst5)) & 8191) as usize];
			self.fRec20[0] = 0.7 * self.fRec20[1] + 0.3 * self.fRec19[1];
			self.fVec6[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec20[0];
			self.fRec19[0] = self.fVec6[((i32::wrapping_sub(self.IOTA0, self.iConst6)) & 8191) as usize];
			self.fRec22[0] = 0.7 * self.fRec22[1] + 0.3 * self.fRec21[1];
			self.fVec7[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec22[0];
			self.fRec21[0] = self.fVec7[((i32::wrapping_sub(self.IOTA0, self.iConst7)) & 8191) as usize];
			self.fRec24[0] = 0.7 * self.fRec24[1] + 0.3 * self.fRec23[1];
			self.fVec8[(self.IOTA0 & 8191) as usize] = fTemp0 + 0.7 * self.fRec24[0];
			self.fRec23[0] = self.fVec8[((i32::wrapping_sub(self.IOTA0, self.iConst8)) & 8191) as usize];
			let mut fTemp7: F32 = self.fRec9[1] + self.fRec11[1] + self.fRec13[1] + self.fRec15[1] + self.fRec17[1] + self.fRec19[1] + self.fRec21[1] + 0.6 * self.fRec7[1] + self.fRec23[1];
			self.fVec9[(self.IOTA0 & 2047) as usize] = fTemp7;
			self.fRec7[0] = self.fVec9[((i32::wrapping_sub(self.IOTA0, self.iConst9)) & 2047) as usize];
			let mut fRec8: F32 = -(0.6 * fTemp7);
			let mut fTemp8: F32 = self.fRec7[1] + fRec8 + 0.6 * self.fRec5[1];
			self.fVec10[(self.IOTA0 & 2047) as usize] = fTemp8;
			self.fRec5[0] = self.fVec10[((i32::wrapping_sub(self.IOTA0, self.iConst10)) & 2047) as usize];
			let mut fRec6: F32 = -(0.6 * fTemp8);
			let mut fTemp9: F32 = self.fRec5[1] + fRec6 + 0.6 * self.fRec3[1];
			self.fVec11[(self.IOTA0 & 2047) as usize] = fTemp9;
			self.fRec3[0] = self.fVec11[((i32::wrapping_sub(self.IOTA0, self.iConst11)) & 2047) as usize];
			let mut fRec4: F32 = -(0.6 * fTemp9);
			let mut fTemp10: F32 = self.fRec3[1] + fRec4 + 0.6 * self.fRec1[1];
			self.fVec12[(self.IOTA0 & 1023) as usize] = fTemp10;
			self.fRec1[0] = self.fVec12[((i32::wrapping_sub(self.IOTA0, self.iConst12)) & 1023) as usize];
			let mut fRec2: F32 = -(0.6 * fTemp10);
			*output1 = (fRec2 + self.fRec1[1]) as FaustFloat;
			self.IOTA0 = i32::wrapping_add(self.IOTA0, 1);
			self.fRec0[1] = self.fRec0[0];
			self.fRec10[1] = self.fRec10[0];
			self.fRec9[1] = self.fRec9[0];
			self.fRec12[1] = self.fRec12[0];
			self.fRec11[1] = self.fRec11[0];
			self.fRec14[1] = self.fRec14[0];
			self.fRec13[1] = self.fRec13[0];
			self.fRec16[1] = self.fRec16[0];
			self.fRec15[1] = self.fRec15[0];
			self.fRec18[1] = self.fRec18[0];
			self.fRec17[1] = self.fRec17[0];
			self.fRec20[1] = self.fRec20[0];
			self.fRec19[1] = self.fRec19[0];
			self.fRec22[1] = self.fRec22[0];
			self.fRec21[1] = self.fRec21[0];
			self.fRec24[1] = self.fRec24[0];
			self.fRec23[1] = self.fRec23[0];
			self.fRec7[1] = self.fRec7[0];
			self.fRec5[1] = self.fRec5[0];
			self.fRec3[1] = self.fRec3[0];
			self.fRec1[1] = self.fRec1[0];
		}
		
	}

}

#[cfg(not(target_arch = "wasm32"))] // Compile ffi bindings only on non-wasm targets
mod ffi {
	use core::ffi::c_float;
	// Conditionally compile the link attribute only on non-Windows platforms
	#[cfg_attr(not(target_os = "windows"), link(name = "m"))]
	unsafe extern "C" {
		pub fn remainderf(from: c_float, to: c_float) -> c_float;
		pub fn rintf(val: c_float) -> c_float;
	}
}
fn remainderf(from: f32, to: f32) -> f32 {
	#[cfg(not(target_arch = "wasm32"))] // non-wasm targets use ffi bindings
	unsafe { ffi::remainderf(from, to) }
	#[cfg(target_arch = "wasm32")] // wasm relies on libm
	libm::remainderf(from, to)
}
fn rintf(val: f32) -> f32 {
	#[cfg(not(target_arch = "wasm32"))] // non-wasm targets use ffi bindings
	unsafe { ffi::rintf(val) }
	#[cfg(target_arch = "wasm32")] // wasm relies on libm
	libm::rintf(val)
}

impl FaustDsp for Tap8Fx {
	type T = FaustFloat;
	fn new() -> Self where Self: Sized {
		Self::new()
	}
	fn metadata(&self, m: &mut dyn Meta) {
		self.metadata(m)
	}
	fn get_sample_rate(&self) -> i32 {
		self.get_sample_rate()
	}
	fn get_num_inputs(&self) -> i32 {
		FAUST_INPUTS as i32
	}
	fn get_num_outputs(&self) -> i32 {
		FAUST_OUTPUTS as i32
	}
	fn class_init(sample_rate: i32) where Self: Sized {
		Self::class_init(sample_rate);
	}
	fn instance_reset_params(&mut self) {
		self.instance_reset_params()
	}
	fn instance_clear(&mut self) {
		self.instance_clear()
	}
	fn instance_constants(&mut self, sample_rate: i32) {
		self.instance_constants(sample_rate)
	}
	fn instance_init(&mut self, sample_rate: i32) {
		self.instance_init(sample_rate)
	}
	fn init(&mut self, sample_rate: i32) {
		self.init(sample_rate)
	}
	fn build_user_interface(&self, ui_interface: &mut dyn UI<Self::T>) {
		self.build_user_interface(ui_interface)
	}
	fn build_user_interface_static(ui_interface: &mut dyn UI<Self::T>) where Self: Sized {
		Self::build_user_interface_static(ui_interface);
	}
	fn get_param(&self, param: ParamIndex) -> Option<Self::T> {
		self.get_param(param)
	}
	fn set_param(&mut self, param: ParamIndex, value: Self::T) {
		self.set_param(param, value)
	}
	fn compute(&mut self, count: i32, inputs: &[&[Self::T]], outputs: &mut [&mut [Self::T]]) {
		self.compute(count as usize, inputs, outputs)
	}
}
