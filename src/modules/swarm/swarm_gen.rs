/* ------------------------------------------------------------
author: "doo-nothing / AU Supply"
license: "AGPL-3.0-or-later"
name: "swarm"
Code generated with Faust 2.85.5 (https://faust.grame.fr)
Compilation options: -lang rust -fpga-mem-th 4 -ct 1 -cn Swarm -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0
------------------------------------------------------------ */

#[repr(C)]
pub struct Swarm {
	fSampleRate: i32,
	fConst0: F32,
	fConst1: F32,
	fHslider0: FaustFloat,
	fConst2: F32,
	iVec0: [i32;2],
	fRec0: [F32;2],
	fConst3: F32,
	fHslider1: FaustFloat,
	fRec5: [F32;2],
	fHslider2: FaustFloat,
	fRec6: [F32;2],
	fConst4: F32,
	fRec8: [F32;2],
	fRec3: [F32;2],
	fConst5: F32,
	fRec11: [F32;2],
	fRec9: [F32;2],
	fConst6: F32,
	fRec14: [F32;2],
	fRec12: [F32;2],
	fConst7: F32,
	fRec17: [F32;2],
	fRec15: [F32;2],
	fConst8: F32,
	fRec20: [F32;2],
	fRec18: [F32;2],
	fConst9: F32,
	fRec23: [F32;2],
	fRec21: [F32;2],
	fConst10: F32,
	fRec26: [F32;2],
	fRec24: [F32;2],
	fHslider3: FaustFloat,
	fRec27: [F32;2],
	fConst11: F32,
	fHslider4: FaustFloat,
	fRec28: [F32;2],
	fRec2: [F32;3],
	fRec1: [F32;3],
}



pub struct SwarmSIG0 {
	iVec1: [i32;2],
	iRec7: [i32;2],
	fSampleRate: i32,
}

impl SwarmSIG0 {
	
	fn get_num_inputsSwarmSIG0(&self) -> i32 {
		return 0;
	}
	fn get_num_outputsSwarmSIG0(&self) -> i32 {
		return 1;
	}
	
	pub fn instance_initSwarmSIG0(&mut self, sample_rate: i32) {
		self.fSampleRate = sample_rate;
		for l4 in 0..2 {
			self.iVec1[l4 as usize] = 0;
		}
		for l5 in 0..2 {
			self.iRec7[l5 as usize] = 0;
		}
	}
	
	pub fn fillSwarmSIG0(&mut self, count: i32, table: &mut[F32]) {
		for i1 in 0..count {
			self.iVec1[0] = 1;
			self.iRec7[0] = (i32::wrapping_add(self.iVec1[1], self.iRec7[1])) % 65536;
			table[i1 as usize] = F32::sin(9.58738e-05 * (self.iRec7[0]) as F32);
			self.iVec1[1] = self.iVec1[0];
			self.iRec7[1] = self.iRec7[0];
		}
	}

}


pub fn newSwarmSIG0() -> SwarmSIG0 { 
	SwarmSIG0 {
		iVec1: [0;2],
		iRec7: [0;2],
		fSampleRate: 0,
	}
}
static ftbl0SwarmSIG0: std::sync::RwLock<[F32;65536]>  = std::sync::RwLock::new([0.0;65536]);
fn Swarm_faustpower2_f(value: F32) -> F32 {
	return value * value;
}
pub const FAUST_INPUTS: usize = 0;
pub const FAUST_OUTPUTS: usize = 1;
pub const FAUST_ACTIVES: usize = 5;
pub const FAUST_PASSIVES: usize = 0;

impl Swarm {
		
	pub fn new() -> Swarm { 
		Swarm {
			fSampleRate: 0,
			fConst0: 0.0,
			fConst1: 0.0,
			fHslider0: 0.0,
			fConst2: 0.0,
			iVec0: [0;2],
			fRec0: [0.0;2],
			fConst3: 0.0,
			fHslider1: 0.0,
			fRec5: [0.0;2],
			fHslider2: 0.0,
			fRec6: [0.0;2],
			fConst4: 0.0,
			fRec8: [0.0;2],
			fRec3: [0.0;2],
			fConst5: 0.0,
			fRec11: [0.0;2],
			fRec9: [0.0;2],
			fConst6: 0.0,
			fRec14: [0.0;2],
			fRec12: [0.0;2],
			fConst7: 0.0,
			fRec17: [0.0;2],
			fRec15: [0.0;2],
			fConst8: 0.0,
			fRec20: [0.0;2],
			fRec18: [0.0;2],
			fConst9: 0.0,
			fRec23: [0.0;2],
			fRec21: [0.0;2],
			fConst10: 0.0,
			fRec26: [0.0;2],
			fRec24: [0.0;2],
			fHslider3: 0.0,
			fRec27: [0.0;2],
			fConst11: 0.0,
			fHslider4: 0.0,
			fRec28: [0.0;2],
			fRec2: [0.0;3],
			fRec1: [0.0;3],
		}
	}
	pub fn metadata(&self, m: &mut dyn Meta) { 
		m.declare("author", r"doo-nothing / AU Supply");
		m.declare("basics.lib/name", r"Faust Basic Element Library");
		m.declare("basics.lib/version", r"1.22.0");
		m.declare("compile_options", r"-lang rust -fpga-mem-th 4 -ct 1 -cn Swarm -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0");
		m.declare("filename", r"swarm.dsp");
		m.declare("filters.lib/fir:author", r"Julius O. Smith III");
		m.declare("filters.lib/fir:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/fir:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/iir:author", r"Julius O. Smith III");
		m.declare("filters.lib/iir:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/iir:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/lowpass0_highpass1", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/name", r"Faust Filters Library");
		m.declare("filters.lib/tf2:author", r"Julius O. Smith III");
		m.declare("filters.lib/tf2:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/tf2:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/tf2s:author", r"Julius O. Smith III");
		m.declare("filters.lib/tf2s:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/tf2s:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/version", r"1.7.1");
		m.declare("license", r"AGPL-3.0-or-later");
		m.declare("maths.lib/author", r"GRAME");
		m.declare("maths.lib/copyright", r"GRAME");
		m.declare("maths.lib/license", r"LGPL with exception");
		m.declare("maths.lib/name", r"Faust Math Library");
		m.declare("maths.lib/version", r"2.9.0");
		m.declare("name", r"swarm");
		m.declare("oscillators.lib/name", r"Faust Oscillator Library");
		m.declare("oscillators.lib/saw2ptr:author", r"Julius O. Smith III");
		m.declare("oscillators.lib/saw2ptr:license", r"STK-4.3");
		m.declare("oscillators.lib/version", r"1.7.0");
		m.declare("platform.lib/name", r"Generic Platform Library");
		m.declare("platform.lib/version", r"1.3.0");
		m.declare("signals.lib/name", r"Faust Routing Library");
		m.declare("signals.lib/version", r"1.6.0");
		m.declare("vaeffects.lib/moog_vcf_2b:author", r"Julius O. Smith III");
		m.declare("vaeffects.lib/moog_vcf_2b:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("vaeffects.lib/moog_vcf_2b:license", r"MIT-style STK-4.3 license");
		m.declare("vaeffects.lib/name", r"Faust Virtual Analog Filter Effect Library");
		m.declare("vaeffects.lib/version", r"1.5.0");
	}

	pub fn get_sample_rate(&self) -> i32 { self.fSampleRate as i32}
	
	pub fn class_init(sample_rate: i32) {
		// Obtaining locks on 1 static var(s)
		let mut ftbl0SwarmSIG0_guard = ftbl0SwarmSIG0.write().unwrap();
		let mut sig0: SwarmSIG0 = newSwarmSIG0();
		sig0.instance_initSwarmSIG0(sample_rate);
		sig0.fillSwarmSIG0(65536, ftbl0SwarmSIG0_guard.as_mut());
	}
	pub fn instance_reset_params(&mut self) {
		self.fHslider0 = (0.8) as FaustFloat;
		self.fHslider1 = (1.1e+02) as FaustFloat;
		self.fHslider2 = (0.3) as FaustFloat;
		self.fHslider3 = (0.25) as FaustFloat;
		self.fHslider4 = (0.5) as FaustFloat;
	}
	pub fn instance_clear(&mut self) {
		for l0 in 0..2 {
			self.iVec0[l0 as usize] = 0;
		}
		for l1 in 0..2 {
			self.fRec0[l1 as usize] = 0.0;
		}
		for l2 in 0..2 {
			self.fRec5[l2 as usize] = 0.0;
		}
		for l3 in 0..2 {
			self.fRec6[l3 as usize] = 0.0;
		}
		for l6 in 0..2 {
			self.fRec8[l6 as usize] = 0.0;
		}
		for l7 in 0..2 {
			self.fRec3[l7 as usize] = 0.0;
		}
		for l8 in 0..2 {
			self.fRec11[l8 as usize] = 0.0;
		}
		for l9 in 0..2 {
			self.fRec9[l9 as usize] = 0.0;
		}
		for l10 in 0..2 {
			self.fRec14[l10 as usize] = 0.0;
		}
		for l11 in 0..2 {
			self.fRec12[l11 as usize] = 0.0;
		}
		for l12 in 0..2 {
			self.fRec17[l12 as usize] = 0.0;
		}
		for l13 in 0..2 {
			self.fRec15[l13 as usize] = 0.0;
		}
		for l14 in 0..2 {
			self.fRec20[l14 as usize] = 0.0;
		}
		for l15 in 0..2 {
			self.fRec18[l15 as usize] = 0.0;
		}
		for l16 in 0..2 {
			self.fRec23[l16 as usize] = 0.0;
		}
		for l17 in 0..2 {
			self.fRec21[l17 as usize] = 0.0;
		}
		for l18 in 0..2 {
			self.fRec26[l18 as usize] = 0.0;
		}
		for l19 in 0..2 {
			self.fRec24[l19 as usize] = 0.0;
		}
		for l20 in 0..2 {
			self.fRec27[l20 as usize] = 0.0;
		}
		for l21 in 0..2 {
			self.fRec28[l21 as usize] = 0.0;
		}
		for l22 in 0..3 {
			self.fRec2[l22 as usize] = 0.0;
		}
		for l23 in 0..3 {
			self.fRec1[l23 as usize] = 0.0;
		}
	}
	pub fn instance_constants(&mut self, sample_rate: i32) {
		// Obtaining locks on 1 static var(s)
		let ftbl0SwarmSIG0_guard = ftbl0SwarmSIG0.read().unwrap();
		self.fSampleRate = sample_rate;
		self.fConst0 = F32::min(1.92e+05, F32::max(1.0, (self.fSampleRate) as F32));
		self.fConst1 = 44.1 / self.fConst0;
		self.fConst2 = 1.0 - self.fConst1;
		self.fConst3 = 1.0 / self.fConst0;
		self.fConst4 = 0.148 / self.fConst0;
		self.fConst5 = 0.135 / self.fConst0;
		self.fConst6 = 0.122 / self.fConst0;
		self.fConst7 = 0.109 / self.fConst0;
		self.fConst8 = 0.096 / self.fConst0;
		self.fConst9 = 0.07 / self.fConst0;
		self.fConst10 = 0.083 / self.fConst0;
		self.fConst11 = 3.1415927 / self.fConst0;
	}
	pub fn instance_init(&mut self, sample_rate: i32) {
		self.instance_constants(sample_rate);
		self.instance_reset_params();
		self.instance_clear();
	}
	pub fn init(&mut self, sample_rate: i32) {
		Swarm::class_init(sample_rate);
		self.instance_init(sample_rate);
	}
	
	pub fn build_user_interface(&self, ui_interface: &mut dyn UI<FaustFloat>) {
		Self::build_user_interface_static(ui_interface);
	}
	
	pub fn build_user_interface_static(ui_interface: &mut dyn UI<FaustFloat>) {
		ui_interface.open_vertical_box("swarm");
		ui_interface.add_horizontal_slider("cutoff", ParamIndex(0), 0.5, 0.0, 1.0, 0.001);
		ui_interface.add_horizontal_slider("detune", ParamIndex(1), 0.3, 0.0, 1.0, 0.001);
		ui_interface.add_horizontal_slider("freq", ParamIndex(2), 1.1e+02, 2e+01, 4e+03, 0.01);
		ui_interface.add_horizontal_slider("level", ParamIndex(3), 0.8, 0.0, 1.0, 0.001);
		ui_interface.add_horizontal_slider("res", ParamIndex(4), 0.25, 0.0, 1.0, 0.001);
		ui_interface.close_box();
	}
	
	pub fn get_param(&self, param: ParamIndex) -> Option<FaustFloat> {
		match param.0 {
			3 => Some(self.fHslider0),
			2 => Some(self.fHslider1),
			1 => Some(self.fHslider2),
			4 => Some(self.fHslider3),
			0 => Some(self.fHslider4),
			_ => None,
		}
	}
	
	pub fn set_param(&mut self, param: ParamIndex, value: FaustFloat) {
		match param.0 {
			3 => { self.fHslider0 = value }
			2 => { self.fHslider1 = value }
			1 => { self.fHslider2 = value }
			4 => { self.fHslider3 = value }
			0 => { self.fHslider4 = value }
			_ => {}
		}
	}
	
	pub fn compute(
		&mut self,
		count: usize,
		inputs: &[impl AsRef<[FaustFloat]>],
		outputs: &mut[impl AsMut<[FaustFloat]>],
	) {
		
		// Obtaining locks on 1 static var(s)
		let ftbl0SwarmSIG0_guard = ftbl0SwarmSIG0.read().unwrap();
		let [outputs0, .. ] = outputs.as_mut() else { panic!("wrong number of output buffers"); };
		let outputs0 = outputs0.as_mut()[..count].iter_mut();
		let mut fSlow0: F32 = self.fConst1 * (self.fHslider0) as F32;
		let mut fSlow1: F32 = self.fConst1 * (self.fHslider1) as F32;
		let mut fSlow2: F32 = self.fConst1 * (self.fHslider2) as F32;
		let mut fSlow3: F32 = self.fConst1 * (self.fHslider3) as F32;
		let mut fSlow4: F32 = self.fConst1 * (self.fHslider4) as F32;
		let zipped_iterators = outputs0;
		for output0 in zipped_iterators {
			self.iVec0[0] = 1;
			self.fRec0[0] = fSlow0 + self.fConst2 * self.fRec0[1];
			self.fRec5[0] = fSlow1 + self.fConst2 * self.fRec5[1];
			self.fRec6[0] = fSlow2 + self.fConst2 * self.fRec6[1];
			let mut iTemp0: i32 = i32::wrapping_sub(1, self.iVec0[1]);
			let mut fTemp1: F32 = (if iTemp0 != 0 {0.0} else {self.fConst4 + self.fRec8[1]});
			self.fRec8[0] = fTemp1 - F32::floor(fTemp1);
			let mut fTemp2: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (self.fRec6[0] * (0.0012 * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec8[0]) as i32, 65535))) as usize] + 0.014) + 1.0)));
			let mut fTemp3: F32 = self.fRec3[1] + self.fConst3 * fTemp2;
			let mut fTemp4: F32 = fTemp3 + -1.0;
			let mut iTemp5: i32 = (fTemp4 < 0.0) as i32;
			self.fRec3[0] = (if iTemp5 != 0 {fTemp3} else {fTemp4});
			let mut fRec4: F32 = (if iTemp5 != 0 {fTemp3} else {fTemp3 + fTemp4 * (1.0 - self.fConst0 / fTemp2)});
			let mut fTemp6: F32 = (if iTemp0 != 0 {0.0} else {self.fConst5 + self.fRec11[1]});
			self.fRec11[0] = fTemp6 - F32::floor(fTemp6);
			let mut fTemp7: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (self.fRec6[0] * (0.0012 * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec11[0]) as i32, 65535))) as usize] + 0.009333333) + 1.0)));
			let mut fTemp8: F32 = self.fRec9[1] + self.fConst3 * fTemp7;
			let mut fTemp9: F32 = fTemp8 + -1.0;
			let mut iTemp10: i32 = (fTemp9 < 0.0) as i32;
			self.fRec9[0] = (if iTemp10 != 0 {fTemp8} else {fTemp9});
			let mut fRec10: F32 = (if iTemp10 != 0 {fTemp8} else {fTemp8 + fTemp9 * (1.0 - self.fConst0 / fTemp7)});
			let mut fTemp11: F32 = (if iTemp0 != 0 {0.0} else {self.fConst6 + self.fRec14[1]});
			self.fRec14[0] = fTemp11 - F32::floor(fTemp11);
			let mut fTemp12: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (self.fRec6[0] * (0.0012 * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec14[0]) as i32, 65535))) as usize] + 0.0046666665) + 1.0)));
			let mut fTemp13: F32 = self.fRec12[1] + self.fConst3 * fTemp12;
			let mut fTemp14: F32 = fTemp13 + -1.0;
			let mut iTemp15: i32 = (fTemp14 < 0.0) as i32;
			self.fRec12[0] = (if iTemp15 != 0 {fTemp13} else {fTemp14});
			let mut fRec13: F32 = (if iTemp15 != 0 {fTemp13} else {fTemp13 + fTemp14 * (1.0 - self.fConst0 / fTemp12)});
			let mut fTemp16: F32 = (if iTemp0 != 0 {0.0} else {self.fConst7 + self.fRec17[1]});
			self.fRec17[0] = fTemp16 - F32::floor(fTemp16);
			let mut fTemp17: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (0.0012 * self.fRec6[0] * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec17[0]) as i32, 65535))) as usize] + 1.0)));
			let mut fTemp18: F32 = self.fRec15[1] + self.fConst3 * fTemp17;
			let mut fTemp19: F32 = fTemp18 + -1.0;
			let mut iTemp20: i32 = (fTemp19 < 0.0) as i32;
			self.fRec15[0] = (if iTemp20 != 0 {fTemp18} else {fTemp19});
			let mut fRec16: F32 = (if iTemp20 != 0 {fTemp18} else {fTemp18 + fTemp19 * (1.0 - self.fConst0 / fTemp17)});
			let mut fTemp21: F32 = (if iTemp0 != 0 {0.0} else {self.fConst8 + self.fRec20[1]});
			self.fRec20[0] = fTemp21 - F32::floor(fTemp21);
			let mut fTemp22: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (self.fRec6[0] * (0.0012 * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec20[0]) as i32, 65535))) as usize] + -0.0046666665) + 1.0)));
			let mut fTemp23: F32 = self.fRec18[1] + self.fConst3 * fTemp22;
			let mut fTemp24: F32 = fTemp23 + -1.0;
			let mut iTemp25: i32 = (fTemp24 < 0.0) as i32;
			self.fRec18[0] = (if iTemp25 != 0 {fTemp23} else {fTemp24});
			let mut fRec19: F32 = (if iTemp25 != 0 {fTemp23} else {fTemp23 + fTemp24 * (1.0 - self.fConst0 / fTemp22)});
			let mut fTemp26: F32 = (if iTemp0 != 0 {0.0} else {self.fConst9 + self.fRec23[1]});
			self.fRec23[0] = fTemp26 - F32::floor(fTemp26);
			let mut fTemp27: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (self.fRec6[0] * (0.0012 * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec23[0]) as i32, 65535))) as usize] + -0.014) + 1.0)));
			let mut fTemp28: F32 = self.fRec21[1] + self.fConst3 * fTemp27;
			let mut fTemp29: F32 = fTemp28 + -1.0;
			let mut iTemp30: i32 = (fTemp29 < 0.0) as i32;
			self.fRec21[0] = (if iTemp30 != 0 {fTemp28} else {fTemp29});
			let mut fRec22: F32 = (if iTemp30 != 0 {fTemp28} else {fTemp28 + fTemp29 * (1.0 - self.fConst0 / fTemp27)});
			let mut fTemp31: F32 = (if iTemp0 != 0 {0.0} else {self.fConst10 + self.fRec26[1]});
			self.fRec26[0] = fTemp31 - F32::floor(fTemp31);
			let mut fTemp32: F32 = F32::max(1.1920929e-07, F32::abs(self.fRec5[0] * (self.fRec6[0] * (0.0012 * ftbl0SwarmSIG0_guard[(core::cmp::max(0, core::cmp::min((65536.0 * self.fRec26[0]) as i32, 65535))) as usize] + -0.009333333) + 1.0)));
			let mut fTemp33: F32 = self.fRec24[1] + self.fConst3 * fTemp32;
			let mut fTemp34: F32 = fTemp33 + -1.0;
			let mut iTemp35: i32 = (fTemp34 < 0.0) as i32;
			self.fRec24[0] = (if iTemp35 != 0 {fTemp33} else {fTemp34});
			let mut fRec25: F32 = (if iTemp35 != 0 {fTemp33} else {fTemp33 + fTemp34 * (1.0 - self.fConst0 / fTemp32)});
			self.fRec27[0] = fSlow3 + self.fConst2 * self.fRec27[1];
			let mut fTemp36: F32 = F32::min(1.4141995, 1.3010765 * self.fRec27[0]);
			let mut fTemp37: F32 = 1.4142135 * fTemp36;
			let mut fTemp38: F32 = Swarm_faustpower2_f(fTemp36);
			let mut fTemp39: F32 = fTemp37 + fTemp38;
			self.fRec28[0] = fSlow4 + self.fConst2 * self.fRec28[1];
			let mut fTemp40: F32 = F32::tan(self.fConst11 * F32::max(2e+01, F32::min(1e+04, F32::min(6e+01 * F32::powf(2e+02, self.fRec28[0]), 1.4e+04))));
			let mut fTemp41: F32 = 1.0 / fTemp40;
			let mut fTemp42: F32 = fTemp37 + 2.0;
			let mut fTemp43: F32 = 1.0 / Swarm_faustpower2_f(fTemp40);
			let mut fTemp44: F32 = fTemp39 + (fTemp41 + fTemp42) / fTemp40 + 1.0;
			self.fRec2[0] = 0.14285715 * (2.0 * (fRec4 - (1.0 - (fRec10 + (fRec13 - (1.0 - (fRec16 + (fRec19 - (1.0 - (fRec22 + fRec25))))))))) + -1.0) - (self.fRec2[2] * (fTemp39 + (fTemp41 - fTemp42) / fTemp40 + 1.0) + 2.0 * self.fRec2[1] * (fTemp39 + (1.0 - fTemp43))) / fTemp44;
			let mut fTemp45: F32 = 2.0 - fTemp37;
			let mut fTemp46: F32 = 1.0 - fTemp37;
			let mut fTemp47: F32 = fTemp38 + (fTemp41 + fTemp45) / fTemp40 + fTemp46;
			self.fRec1[0] = (self.fRec2[2] + self.fRec2[0] + 2.0 * self.fRec2[1]) / fTemp44 - (self.fRec1[2] * (fTemp38 + (fTemp41 - fTemp45) / fTemp40 + fTemp46) + 2.0 * self.fRec1[1] * (fTemp38 + (1.0 - (fTemp37 + fTemp43)))) / fTemp47;
			*output0 = (self.fRec0[0] * (self.fRec1[2] + self.fRec1[0] + 2.0 * self.fRec1[1]) / fTemp47) as FaustFloat;
			self.iVec0[1] = self.iVec0[0];
			self.fRec0[1] = self.fRec0[0];
			self.fRec5[1] = self.fRec5[0];
			self.fRec6[1] = self.fRec6[0];
			self.fRec8[1] = self.fRec8[0];
			self.fRec3[1] = self.fRec3[0];
			self.fRec11[1] = self.fRec11[0];
			self.fRec9[1] = self.fRec9[0];
			self.fRec14[1] = self.fRec14[0];
			self.fRec12[1] = self.fRec12[0];
			self.fRec17[1] = self.fRec17[0];
			self.fRec15[1] = self.fRec15[0];
			self.fRec20[1] = self.fRec20[0];
			self.fRec18[1] = self.fRec18[0];
			self.fRec23[1] = self.fRec23[0];
			self.fRec21[1] = self.fRec21[0];
			self.fRec26[1] = self.fRec26[0];
			self.fRec24[1] = self.fRec24[0];
			self.fRec27[1] = self.fRec27[0];
			self.fRec28[1] = self.fRec28[0];
			self.fRec2[2] = self.fRec2[1];
			self.fRec2[1] = self.fRec2[0];
			self.fRec1[2] = self.fRec1[1];
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

impl FaustDsp for Swarm {
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
