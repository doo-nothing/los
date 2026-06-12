/* ------------------------------------------------------------
author: "doo-nothing / AU Supply"
license: "AGPL-3.0-or-later"
name: "wasp"
Code generated with Faust 2.85.5 (https://faust.grame.fr)
Compilation options: -lang rust -fpga-mem-th 4 -ct 1 -cn Wasp -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0
------------------------------------------------------------ */

#[repr(C)]
pub struct Wasp {
	fSampleRate: i32,
	fConst0: F32,
	fConst1: F32,
	fHslider0: FaustFloat,
	fConst2: F32,
	fRec0: [F32;2],
	fConst3: F32,
	fConst4: F32,
	fHslider1: FaustFloat,
	fRec6: [F32;2],
	fHslider2: FaustFloat,
	fRec7: [F32;2],
	fRec4: [F32;2],
	fRec5: [F32;2],
	fHslider3: FaustFloat,
	fRec8: [F32;2],
}


fn Wasp_faustpower2_f(value: F32) -> F32 {
	return value * value;
}
pub const FAUST_INPUTS: usize = 1;
pub const FAUST_OUTPUTS: usize = 2;
pub const FAUST_ACTIVES: usize = 4;
pub const FAUST_PASSIVES: usize = 0;

impl Wasp {
		
	pub fn new() -> Wasp { 
		Wasp {
			fSampleRate: 0,
			fConst0: 0.0,
			fConst1: 0.0,
			fHslider0: 0.0,
			fConst2: 0.0,
			fRec0: [0.0;2],
			fConst3: 0.0,
			fConst4: 0.0,
			fHslider1: 0.0,
			fRec6: [0.0;2],
			fHslider2: 0.0,
			fRec7: [0.0;2],
			fRec4: [0.0;2],
			fRec5: [0.0;2],
			fHslider3: 0.0,
			fRec8: [0.0;2],
		}
	}
	pub fn metadata(&self, m: &mut dyn Meta) { 
		m.declare("author", r"doo-nothing / AU Supply");
		m.declare("compile_options", r"-lang rust -fpga-mem-th 4 -ct 1 -cn Wasp -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0");
		m.declare("filename", r"wasp.dsp");
		m.declare("license", r"AGPL-3.0-or-later");
		m.declare("maths.lib/author", r"GRAME");
		m.declare("maths.lib/copyright", r"GRAME");
		m.declare("maths.lib/license", r"LGPL with exception");
		m.declare("maths.lib/name", r"Faust Math Library");
		m.declare("maths.lib/version", r"2.9.0");
		m.declare("misceffects.lib/cubicnl:author", r"Julius O. Smith III");
		m.declare("misceffects.lib/cubicnl:license", r"STK-4.3");
		m.declare("misceffects.lib/name", r"Misc Effects Library");
		m.declare("misceffects.lib/version", r"2.5.2");
		m.declare("name", r"wasp");
		m.declare("platform.lib/name", r"Generic Platform Library");
		m.declare("platform.lib/version", r"1.3.0");
		m.declare("signals.lib/name", r"Faust Routing Library");
		m.declare("signals.lib/version", r"1.6.0");
		m.declare("vaeffects.lib/name", r"Faust Virtual Analog Filter Effect Library");
		m.declare("vaeffects.lib/oberheim:author", r"Eric Tarr");
		m.declare("vaeffects.lib/oberheim:license", r"MIT-style STK-4.3 license");
		m.declare("vaeffects.lib/oberheimBPF:author", r"Eric Tarr");
		m.declare("vaeffects.lib/oberheimBPF:license", r"MIT-style STK-4.3 license");
		m.declare("vaeffects.lib/oberheimHPF:author", r"Eric Tarr");
		m.declare("vaeffects.lib/oberheimHPF:license", r"MIT-style STK-4.3 license");
		m.declare("vaeffects.lib/oberheimLPF:author", r"Eric Tarr");
		m.declare("vaeffects.lib/oberheimLPF:license", r"MIT-style STK-4.3 license");
		m.declare("vaeffects.lib/version", r"1.5.0");
	}

	pub fn get_sample_rate(&self) -> i32 { self.fSampleRate as i32}
	
	pub fn class_init(sample_rate: i32) {
		// Obtaining locks on 0 static var(s)
	}
	pub fn instance_reset_params(&mut self) {
		self.fHslider0 = (0.5) as FaustFloat;
		self.fHslider1 = (0.5) as FaustFloat;
		self.fHslider2 = (0.3) as FaustFloat;
		self.fHslider3 = (0.0) as FaustFloat;
	}
	pub fn instance_clear(&mut self) {
		for l0 in 0..2 {
			self.fRec0[l0 as usize] = 0.0;
		}
		for l1 in 0..2 {
			self.fRec6[l1 as usize] = 0.0;
		}
		for l2 in 0..2 {
			self.fRec7[l2 as usize] = 0.0;
		}
		for l3 in 0..2 {
			self.fRec4[l3 as usize] = 0.0;
		}
		for l4 in 0..2 {
			self.fRec5[l4 as usize] = 0.0;
		}
		for l5 in 0..2 {
			self.fRec8[l5 as usize] = 0.0;
		}
	}
	pub fn instance_constants(&mut self, sample_rate: i32) {
		// Obtaining locks on 0 static var(s)
		self.fSampleRate = sample_rate;
		self.fConst0 = F32::min(1.92e+05, F32::max(1.0, (self.fSampleRate) as F32));
		self.fConst1 = 44.1 / self.fConst0;
		self.fConst2 = 1.0 - self.fConst1;
		self.fConst3 = 6.2831855 / self.fConst0;
		self.fConst4 = 3.0 / self.fConst0;
	}
	pub fn instance_init(&mut self, sample_rate: i32) {
		self.instance_constants(sample_rate);
		self.instance_reset_params();
		self.instance_clear();
	}
	pub fn init(&mut self, sample_rate: i32) {
		Wasp::class_init(sample_rate);
		self.instance_init(sample_rate);
	}
	
	pub fn build_user_interface(&self, ui_interface: &mut dyn UI<FaustFloat>) {
		Self::build_user_interface_static(ui_interface);
	}
	
	pub fn build_user_interface_static(ui_interface: &mut dyn UI<FaustFloat>) {
		ui_interface.open_vertical_box("wasp");
		ui_interface.add_horizontal_slider("dirt", ParamIndex(0), 0.5, 0.0, 1.0, 0.001);
		ui_interface.add_horizontal_slider("freq", ParamIndex(1), 0.5, 0.0, 1.0, 0.001);
		ui_interface.add_horizontal_slider("mix", ParamIndex(2), 0.0, 0.0, 1.0, 0.001);
		ui_interface.add_horizontal_slider("res", ParamIndex(3), 0.3, 0.0, 1.0, 0.001);
		ui_interface.close_box();
	}
	
	pub fn get_param(&self, param: ParamIndex) -> Option<FaustFloat> {
		match param.0 {
			0 => Some(self.fHslider0),
			1 => Some(self.fHslider1),
			3 => Some(self.fHslider2),
			2 => Some(self.fHslider3),
			_ => None,
		}
	}
	
	pub fn set_param(&mut self, param: ParamIndex, value: FaustFloat) {
		match param.0 {
			0 => { self.fHslider0 = value }
			1 => { self.fHslider1 = value }
			3 => { self.fHslider2 = value }
			2 => { self.fHslider3 = value }
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
		let mut fSlow0: F32 = self.fConst1 * (self.fHslider0) as F32;
		let mut fSlow1: F32 = self.fConst1 * (self.fHslider1) as F32;
		let mut fSlow2: F32 = self.fConst1 * (self.fHslider2) as F32;
		let mut fSlow3: F32 = self.fConst1 * (self.fHslider3) as F32;
		let zipped_iterators = inputs0.zip(outputs0).zip(outputs1);
		for ((input0, output0), output1) in zipped_iterators {
			self.fRec0[0] = fSlow0 + self.fConst2 * self.fRec0[1];
			let mut fTemp0: F32 = 1.0 - self.fRec0[0];
			self.fRec6[0] = fSlow1 + self.fConst2 * self.fRec6[1];
			let mut fTemp1: F32 = F32::tan(self.fConst3 * F32::powf(1e+01, self.fConst4 * F32::min(3e+01 * F32::powf(4e+02, self.fRec6[0]), 1.4e+04) + 1.0));
			let mut fTemp2: F32 = (*input0) as F32;
			let mut fTemp3: F32 = 1.5 * self.fRec0[0] + 1.0;
			let mut fTemp4: F32 = 3.0 * self.fRec0[0] + 1.0;
			self.fRec7[0] = fSlow2 + self.fConst2 * self.fRec7[1];
			let mut fTemp5: F32 = 1.0 / (19.5 * self.fRec7[0] + 0.5) + fTemp1;
			let mut fTemp6: F32 = fTemp2 * fTemp0 * fTemp3 + self.fRec0[0] * F32::tanh(fTemp2 * fTemp3 * fTemp4) - (self.fRec4[1] + self.fRec5[1] * fTemp5);
			let mut fTemp7: F32 = fTemp1 * fTemp5 + 1.0;
			let mut fTemp8: F32 = fTemp1 * fTemp6 / fTemp7;
			let mut fTemp9: F32 = F32::max(-1.0, F32::min(1.0, self.fRec5[1] + fTemp8));
			let mut fTemp10: F32 = 1.0 - 0.33333334 * Wasp_faustpower2_f(fTemp9);
			let mut fTemp11: F32 = fTemp1 * fTemp9 * fTemp10;
			let mut fRec1: F32 = self.fRec4[1] + fTemp11;
			let mut fTemp12: F32 = fTemp6 / fTemp7;
			let mut fRec2: F32 = fTemp12;
			let mut fTemp13: F32 = fTemp9 * fTemp10;
			let mut fRec3: F32 = fTemp13;
			self.fRec4[0] = self.fRec4[1] + 2.0 * fTemp11;
			self.fRec5[0] = fTemp8 + fTemp13;
			self.fRec8[0] = fSlow3 + self.fConst2 * self.fRec8[1];
			let mut fTemp14: F32 = fRec1 * (1.0 - self.fRec8[0]) + self.fRec8[0] * fRec2;
			*output0 = (fTemp0 * fTemp14 + self.fRec0[0] * F32::tanh(fTemp4 * fTemp14)) as FaustFloat;
			*output1 = (fTemp0 * fRec3 + self.fRec0[0] * F32::tanh(fRec3 * fTemp4)) as FaustFloat;
			self.fRec0[1] = self.fRec0[0];
			self.fRec6[1] = self.fRec6[0];
			self.fRec7[1] = self.fRec7[0];
			self.fRec4[1] = self.fRec4[0];
			self.fRec5[1] = self.fRec5[0];
			self.fRec8[1] = self.fRec8[0];
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

impl FaustDsp for Wasp {
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
