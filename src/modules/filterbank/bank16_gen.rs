/* ------------------------------------------------------------
author: "doo-nothing / AU Supply"
license: "AGPL-3.0-or-later"
name: "bank16"
Code generated with Faust 2.85.5 (https://faust.grame.fr)
Compilation options: -lang rust -fpga-mem-th 4 -ct 1 -cn Bank16 -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0
------------------------------------------------------------ */

#[repr(C)]
pub struct Bank16 {
	fSampleRate: i32,
	fConst0: F32,
	fConst1: F32,
	fConst2: F32,
	fConst3: F32,
	fConst4: F32,
	fConst5: F32,
	fVec0: [F32;2],
	fRec1: [F32;2],
	fConst6: F32,
	fConst7: F32,
	fRec0: [F32;3],
	fConst8: F32,
	fConst9: F32,
	fConst10: F32,
	fConst11: F32,
	fConst12: F32,
	fConst13: F32,
	fConst14: F32,
	fConst15: F32,
	fConst16: F32,
	fConst17: F32,
	fConst18: F32,
	fConst19: F32,
	fConst20: F32,
	fConst21: F32,
	fConst22: F32,
	fRec4: [F32;3],
	fConst23: F32,
	fConst24: F32,
	fConst25: F32,
	fRec3: [F32;3],
	fConst26: F32,
	fConst27: F32,
	fConst28: F32,
	fRec2: [F32;3],
	fConst29: F32,
	fConst30: F32,
	fConst31: F32,
	fConst32: F32,
	fConst33: F32,
	fConst34: F32,
	fConst35: F32,
	fConst36: F32,
	fConst37: F32,
	fConst38: F32,
	fConst39: F32,
	fConst40: F32,
	fConst41: F32,
	fConst42: F32,
	fConst43: F32,
	fRec7: [F32;3],
	fConst44: F32,
	fConst45: F32,
	fConst46: F32,
	fRec6: [F32;3],
	fConst47: F32,
	fConst48: F32,
	fConst49: F32,
	fRec5: [F32;3],
	fConst50: F32,
	fConst51: F32,
	fConst52: F32,
	fConst53: F32,
	fConst54: F32,
	fConst55: F32,
	fConst56: F32,
	fConst57: F32,
	fConst58: F32,
	fConst59: F32,
	fConst60: F32,
	fConst61: F32,
	fConst62: F32,
	fConst63: F32,
	fConst64: F32,
	fRec10: [F32;3],
	fConst65: F32,
	fConst66: F32,
	fConst67: F32,
	fRec9: [F32;3],
	fConst68: F32,
	fConst69: F32,
	fConst70: F32,
	fRec8: [F32;3],
	fConst71: F32,
	fConst72: F32,
	fConst73: F32,
	fConst74: F32,
	fConst75: F32,
	fConst76: F32,
	fConst77: F32,
	fConst78: F32,
	fConst79: F32,
	fConst80: F32,
	fConst81: F32,
	fConst82: F32,
	fConst83: F32,
	fConst84: F32,
	fConst85: F32,
	fRec13: [F32;3],
	fConst86: F32,
	fConst87: F32,
	fConst88: F32,
	fRec12: [F32;3],
	fConst89: F32,
	fConst90: F32,
	fConst91: F32,
	fRec11: [F32;3],
	fConst92: F32,
	fConst93: F32,
	fConst94: F32,
	fConst95: F32,
	fConst96: F32,
	fConst97: F32,
	fConst98: F32,
	fConst99: F32,
	fConst100: F32,
	fConst101: F32,
	fConst102: F32,
	fConst103: F32,
	fConst104: F32,
	fConst105: F32,
	fConst106: F32,
	fRec16: [F32;3],
	fConst107: F32,
	fConst108: F32,
	fConst109: F32,
	fRec15: [F32;3],
	fConst110: F32,
	fConst111: F32,
	fConst112: F32,
	fRec14: [F32;3],
	fConst113: F32,
	fConst114: F32,
	fConst115: F32,
	fConst116: F32,
	fConst117: F32,
	fConst118: F32,
	fConst119: F32,
	fConst120: F32,
	fConst121: F32,
	fConst122: F32,
	fConst123: F32,
	fConst124: F32,
	fConst125: F32,
	fConst126: F32,
	fConst127: F32,
	fRec19: [F32;3],
	fConst128: F32,
	fConst129: F32,
	fConst130: F32,
	fRec18: [F32;3],
	fConst131: F32,
	fConst132: F32,
	fConst133: F32,
	fRec17: [F32;3],
	fConst134: F32,
	fConst135: F32,
	fConst136: F32,
	fConst137: F32,
	fConst138: F32,
	fConst139: F32,
	fConst140: F32,
	fConst141: F32,
	fConst142: F32,
	fConst143: F32,
	fConst144: F32,
	fConst145: F32,
	fConst146: F32,
	fConst147: F32,
	fConst148: F32,
	fRec22: [F32;3],
	fConst149: F32,
	fConst150: F32,
	fConst151: F32,
	fRec21: [F32;3],
	fConst152: F32,
	fConst153: F32,
	fConst154: F32,
	fRec20: [F32;3],
	fConst155: F32,
	fConst156: F32,
	fConst157: F32,
	fConst158: F32,
	fConst159: F32,
	fConst160: F32,
	fConst161: F32,
	fConst162: F32,
	fConst163: F32,
	fConst164: F32,
	fConst165: F32,
	fConst166: F32,
	fConst167: F32,
	fConst168: F32,
	fConst169: F32,
	fRec25: [F32;3],
	fConst170: F32,
	fConst171: F32,
	fConst172: F32,
	fRec24: [F32;3],
	fConst173: F32,
	fConst174: F32,
	fConst175: F32,
	fRec23: [F32;3],
	fConst176: F32,
	fConst177: F32,
	fConst178: F32,
	fConst179: F32,
	fConst180: F32,
	fConst181: F32,
	fConst182: F32,
	fConst183: F32,
	fConst184: F32,
	fConst185: F32,
	fConst186: F32,
	fConst187: F32,
	fConst188: F32,
	fConst189: F32,
	fConst190: F32,
	fRec28: [F32;3],
	fConst191: F32,
	fConst192: F32,
	fConst193: F32,
	fRec27: [F32;3],
	fConst194: F32,
	fConst195: F32,
	fConst196: F32,
	fRec26: [F32;3],
	fConst197: F32,
	fConst198: F32,
	fConst199: F32,
	fConst200: F32,
	fConst201: F32,
	fConst202: F32,
	fConst203: F32,
	fConst204: F32,
	fConst205: F32,
	fConst206: F32,
	fConst207: F32,
	fConst208: F32,
	fConst209: F32,
	fConst210: F32,
	fConst211: F32,
	fRec31: [F32;3],
	fConst212: F32,
	fConst213: F32,
	fConst214: F32,
	fRec30: [F32;3],
	fConst215: F32,
	fConst216: F32,
	fConst217: F32,
	fRec29: [F32;3],
	fConst218: F32,
	fConst219: F32,
	fConst220: F32,
	fConst221: F32,
	fConst222: F32,
	fConst223: F32,
	fConst224: F32,
	fConst225: F32,
	fConst226: F32,
	fConst227: F32,
	fConst228: F32,
	fConst229: F32,
	fConst230: F32,
	fConst231: F32,
	fConst232: F32,
	fRec34: [F32;3],
	fConst233: F32,
	fConst234: F32,
	fConst235: F32,
	fRec33: [F32;3],
	fConst236: F32,
	fConst237: F32,
	fConst238: F32,
	fRec32: [F32;3],
	fConst239: F32,
	fConst240: F32,
	fConst241: F32,
	fConst242: F32,
	fConst243: F32,
	fConst244: F32,
	fConst245: F32,
	fConst246: F32,
	fConst247: F32,
	fConst248: F32,
	fConst249: F32,
	fConst250: F32,
	fConst251: F32,
	fConst252: F32,
	fConst253: F32,
	fRec37: [F32;3],
	fConst254: F32,
	fConst255: F32,
	fConst256: F32,
	fRec36: [F32;3],
	fConst257: F32,
	fConst258: F32,
	fConst259: F32,
	fRec35: [F32;3],
	fConst260: F32,
	fConst261: F32,
	fConst262: F32,
	fConst263: F32,
	fConst264: F32,
	fConst265: F32,
	fConst266: F32,
	fConst267: F32,
	fConst268: F32,
	fConst269: F32,
	fConst270: F32,
	fConst271: F32,
	fConst272: F32,
	fConst273: F32,
	fConst274: F32,
	fRec40: [F32;3],
	fConst275: F32,
	fConst276: F32,
	fConst277: F32,
	fRec39: [F32;3],
	fConst278: F32,
	fConst279: F32,
	fConst280: F32,
	fRec38: [F32;3],
	fConst281: F32,
	fConst282: F32,
	fConst283: F32,
	fConst284: F32,
	fConst285: F32,
	fConst286: F32,
	fConst287: F32,
	fConst288: F32,
	fConst289: F32,
	fConst290: F32,
	fConst291: F32,
	fConst292: F32,
	fConst293: F32,
	fConst294: F32,
	fConst295: F32,
	fRec43: [F32;3],
	fConst296: F32,
	fConst297: F32,
	fConst298: F32,
	fRec42: [F32;3],
	fConst299: F32,
	fConst300: F32,
	fConst301: F32,
	fRec41: [F32;3],
	fConst302: F32,
	fConst303: F32,
	fConst304: F32,
	fConst305: F32,
	fConst306: F32,
	fConst307: F32,
	fConst308: F32,
	fRec45: [F32;2],
	fConst309: F32,
	fConst310: F32,
	fConst311: F32,
	fRec44: [F32;3],
}


fn Bank16_faustpower2_f(value: F32) -> F32 {
	return value * value;
}
pub const FAUST_INPUTS: usize = 1;
pub const FAUST_OUTPUTS: usize = 16;
pub const FAUST_ACTIVES: usize = 0;
pub const FAUST_PASSIVES: usize = 0;

impl Bank16 {
		
	pub fn new() -> Bank16 { 
		Bank16 {
			fSampleRate: 0,
			fConst0: 0.0,
			fConst1: 0.0,
			fConst2: 0.0,
			fConst3: 0.0,
			fConst4: 0.0,
			fConst5: 0.0,
			fVec0: [0.0;2],
			fRec1: [0.0;2],
			fConst6: 0.0,
			fConst7: 0.0,
			fRec0: [0.0;3],
			fConst8: 0.0,
			fConst9: 0.0,
			fConst10: 0.0,
			fConst11: 0.0,
			fConst12: 0.0,
			fConst13: 0.0,
			fConst14: 0.0,
			fConst15: 0.0,
			fConst16: 0.0,
			fConst17: 0.0,
			fConst18: 0.0,
			fConst19: 0.0,
			fConst20: 0.0,
			fConst21: 0.0,
			fConst22: 0.0,
			fRec4: [0.0;3],
			fConst23: 0.0,
			fConst24: 0.0,
			fConst25: 0.0,
			fRec3: [0.0;3],
			fConst26: 0.0,
			fConst27: 0.0,
			fConst28: 0.0,
			fRec2: [0.0;3],
			fConst29: 0.0,
			fConst30: 0.0,
			fConst31: 0.0,
			fConst32: 0.0,
			fConst33: 0.0,
			fConst34: 0.0,
			fConst35: 0.0,
			fConst36: 0.0,
			fConst37: 0.0,
			fConst38: 0.0,
			fConst39: 0.0,
			fConst40: 0.0,
			fConst41: 0.0,
			fConst42: 0.0,
			fConst43: 0.0,
			fRec7: [0.0;3],
			fConst44: 0.0,
			fConst45: 0.0,
			fConst46: 0.0,
			fRec6: [0.0;3],
			fConst47: 0.0,
			fConst48: 0.0,
			fConst49: 0.0,
			fRec5: [0.0;3],
			fConst50: 0.0,
			fConst51: 0.0,
			fConst52: 0.0,
			fConst53: 0.0,
			fConst54: 0.0,
			fConst55: 0.0,
			fConst56: 0.0,
			fConst57: 0.0,
			fConst58: 0.0,
			fConst59: 0.0,
			fConst60: 0.0,
			fConst61: 0.0,
			fConst62: 0.0,
			fConst63: 0.0,
			fConst64: 0.0,
			fRec10: [0.0;3],
			fConst65: 0.0,
			fConst66: 0.0,
			fConst67: 0.0,
			fRec9: [0.0;3],
			fConst68: 0.0,
			fConst69: 0.0,
			fConst70: 0.0,
			fRec8: [0.0;3],
			fConst71: 0.0,
			fConst72: 0.0,
			fConst73: 0.0,
			fConst74: 0.0,
			fConst75: 0.0,
			fConst76: 0.0,
			fConst77: 0.0,
			fConst78: 0.0,
			fConst79: 0.0,
			fConst80: 0.0,
			fConst81: 0.0,
			fConst82: 0.0,
			fConst83: 0.0,
			fConst84: 0.0,
			fConst85: 0.0,
			fRec13: [0.0;3],
			fConst86: 0.0,
			fConst87: 0.0,
			fConst88: 0.0,
			fRec12: [0.0;3],
			fConst89: 0.0,
			fConst90: 0.0,
			fConst91: 0.0,
			fRec11: [0.0;3],
			fConst92: 0.0,
			fConst93: 0.0,
			fConst94: 0.0,
			fConst95: 0.0,
			fConst96: 0.0,
			fConst97: 0.0,
			fConst98: 0.0,
			fConst99: 0.0,
			fConst100: 0.0,
			fConst101: 0.0,
			fConst102: 0.0,
			fConst103: 0.0,
			fConst104: 0.0,
			fConst105: 0.0,
			fConst106: 0.0,
			fRec16: [0.0;3],
			fConst107: 0.0,
			fConst108: 0.0,
			fConst109: 0.0,
			fRec15: [0.0;3],
			fConst110: 0.0,
			fConst111: 0.0,
			fConst112: 0.0,
			fRec14: [0.0;3],
			fConst113: 0.0,
			fConst114: 0.0,
			fConst115: 0.0,
			fConst116: 0.0,
			fConst117: 0.0,
			fConst118: 0.0,
			fConst119: 0.0,
			fConst120: 0.0,
			fConst121: 0.0,
			fConst122: 0.0,
			fConst123: 0.0,
			fConst124: 0.0,
			fConst125: 0.0,
			fConst126: 0.0,
			fConst127: 0.0,
			fRec19: [0.0;3],
			fConst128: 0.0,
			fConst129: 0.0,
			fConst130: 0.0,
			fRec18: [0.0;3],
			fConst131: 0.0,
			fConst132: 0.0,
			fConst133: 0.0,
			fRec17: [0.0;3],
			fConst134: 0.0,
			fConst135: 0.0,
			fConst136: 0.0,
			fConst137: 0.0,
			fConst138: 0.0,
			fConst139: 0.0,
			fConst140: 0.0,
			fConst141: 0.0,
			fConst142: 0.0,
			fConst143: 0.0,
			fConst144: 0.0,
			fConst145: 0.0,
			fConst146: 0.0,
			fConst147: 0.0,
			fConst148: 0.0,
			fRec22: [0.0;3],
			fConst149: 0.0,
			fConst150: 0.0,
			fConst151: 0.0,
			fRec21: [0.0;3],
			fConst152: 0.0,
			fConst153: 0.0,
			fConst154: 0.0,
			fRec20: [0.0;3],
			fConst155: 0.0,
			fConst156: 0.0,
			fConst157: 0.0,
			fConst158: 0.0,
			fConst159: 0.0,
			fConst160: 0.0,
			fConst161: 0.0,
			fConst162: 0.0,
			fConst163: 0.0,
			fConst164: 0.0,
			fConst165: 0.0,
			fConst166: 0.0,
			fConst167: 0.0,
			fConst168: 0.0,
			fConst169: 0.0,
			fRec25: [0.0;3],
			fConst170: 0.0,
			fConst171: 0.0,
			fConst172: 0.0,
			fRec24: [0.0;3],
			fConst173: 0.0,
			fConst174: 0.0,
			fConst175: 0.0,
			fRec23: [0.0;3],
			fConst176: 0.0,
			fConst177: 0.0,
			fConst178: 0.0,
			fConst179: 0.0,
			fConst180: 0.0,
			fConst181: 0.0,
			fConst182: 0.0,
			fConst183: 0.0,
			fConst184: 0.0,
			fConst185: 0.0,
			fConst186: 0.0,
			fConst187: 0.0,
			fConst188: 0.0,
			fConst189: 0.0,
			fConst190: 0.0,
			fRec28: [0.0;3],
			fConst191: 0.0,
			fConst192: 0.0,
			fConst193: 0.0,
			fRec27: [0.0;3],
			fConst194: 0.0,
			fConst195: 0.0,
			fConst196: 0.0,
			fRec26: [0.0;3],
			fConst197: 0.0,
			fConst198: 0.0,
			fConst199: 0.0,
			fConst200: 0.0,
			fConst201: 0.0,
			fConst202: 0.0,
			fConst203: 0.0,
			fConst204: 0.0,
			fConst205: 0.0,
			fConst206: 0.0,
			fConst207: 0.0,
			fConst208: 0.0,
			fConst209: 0.0,
			fConst210: 0.0,
			fConst211: 0.0,
			fRec31: [0.0;3],
			fConst212: 0.0,
			fConst213: 0.0,
			fConst214: 0.0,
			fRec30: [0.0;3],
			fConst215: 0.0,
			fConst216: 0.0,
			fConst217: 0.0,
			fRec29: [0.0;3],
			fConst218: 0.0,
			fConst219: 0.0,
			fConst220: 0.0,
			fConst221: 0.0,
			fConst222: 0.0,
			fConst223: 0.0,
			fConst224: 0.0,
			fConst225: 0.0,
			fConst226: 0.0,
			fConst227: 0.0,
			fConst228: 0.0,
			fConst229: 0.0,
			fConst230: 0.0,
			fConst231: 0.0,
			fConst232: 0.0,
			fRec34: [0.0;3],
			fConst233: 0.0,
			fConst234: 0.0,
			fConst235: 0.0,
			fRec33: [0.0;3],
			fConst236: 0.0,
			fConst237: 0.0,
			fConst238: 0.0,
			fRec32: [0.0;3],
			fConst239: 0.0,
			fConst240: 0.0,
			fConst241: 0.0,
			fConst242: 0.0,
			fConst243: 0.0,
			fConst244: 0.0,
			fConst245: 0.0,
			fConst246: 0.0,
			fConst247: 0.0,
			fConst248: 0.0,
			fConst249: 0.0,
			fConst250: 0.0,
			fConst251: 0.0,
			fConst252: 0.0,
			fConst253: 0.0,
			fRec37: [0.0;3],
			fConst254: 0.0,
			fConst255: 0.0,
			fConst256: 0.0,
			fRec36: [0.0;3],
			fConst257: 0.0,
			fConst258: 0.0,
			fConst259: 0.0,
			fRec35: [0.0;3],
			fConst260: 0.0,
			fConst261: 0.0,
			fConst262: 0.0,
			fConst263: 0.0,
			fConst264: 0.0,
			fConst265: 0.0,
			fConst266: 0.0,
			fConst267: 0.0,
			fConst268: 0.0,
			fConst269: 0.0,
			fConst270: 0.0,
			fConst271: 0.0,
			fConst272: 0.0,
			fConst273: 0.0,
			fConst274: 0.0,
			fRec40: [0.0;3],
			fConst275: 0.0,
			fConst276: 0.0,
			fConst277: 0.0,
			fRec39: [0.0;3],
			fConst278: 0.0,
			fConst279: 0.0,
			fConst280: 0.0,
			fRec38: [0.0;3],
			fConst281: 0.0,
			fConst282: 0.0,
			fConst283: 0.0,
			fConst284: 0.0,
			fConst285: 0.0,
			fConst286: 0.0,
			fConst287: 0.0,
			fConst288: 0.0,
			fConst289: 0.0,
			fConst290: 0.0,
			fConst291: 0.0,
			fConst292: 0.0,
			fConst293: 0.0,
			fConst294: 0.0,
			fConst295: 0.0,
			fRec43: [0.0;3],
			fConst296: 0.0,
			fConst297: 0.0,
			fConst298: 0.0,
			fRec42: [0.0;3],
			fConst299: 0.0,
			fConst300: 0.0,
			fConst301: 0.0,
			fRec41: [0.0;3],
			fConst302: 0.0,
			fConst303: 0.0,
			fConst304: 0.0,
			fConst305: 0.0,
			fConst306: 0.0,
			fConst307: 0.0,
			fConst308: 0.0,
			fRec45: [0.0;2],
			fConst309: 0.0,
			fConst310: 0.0,
			fConst311: 0.0,
			fRec44: [0.0;3],
		}
	}
	pub fn metadata(&self, m: &mut dyn Meta) { 
		m.declare("author", r"doo-nothing / AU Supply");
		m.declare("basics.lib/name", r"Faust Basic Element Library");
		m.declare("basics.lib/version", r"1.22.0");
		m.declare("compile_options", r"-lang rust -fpga-mem-th 4 -ct 1 -cn Bank16 -es 1 -mcd 16 -mdd 1024 -mdy 33 -single -ftz 0");
		m.declare("filename", r"bank16.dsp");
		m.declare("filters.lib/fir:author", r"Julius O. Smith III");
		m.declare("filters.lib/fir:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/fir:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/highpass:author", r"Julius O. Smith III");
		m.declare("filters.lib/highpass:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/iir:author", r"Julius O. Smith III");
		m.declare("filters.lib/iir:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/iir:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/lowpass0_highpass1", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/lowpass0_highpass1:author", r"Julius O. Smith III");
		m.declare("filters.lib/lowpass:author", r"Julius O. Smith III");
		m.declare("filters.lib/lowpass:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/lowpass:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/name", r"Faust Filters Library");
		m.declare("filters.lib/resonbp:author", r"Julius O. Smith III");
		m.declare("filters.lib/resonbp:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/resonbp:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/tf1:author", r"Julius O. Smith III");
		m.declare("filters.lib/tf1:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/tf1:license", r"MIT-style STK-4.3 license");
		m.declare("filters.lib/tf1s:author", r"Julius O. Smith III");
		m.declare("filters.lib/tf1s:copyright", r"Copyright (C) 2003-2019 by Julius O. Smith III <jos@ccrma.stanford.edu>");
		m.declare("filters.lib/tf1s:license", r"MIT-style STK-4.3 license");
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
		m.declare("name", r"bank16");
		m.declare("platform.lib/name", r"Generic Platform Library");
		m.declare("platform.lib/version", r"1.3.0");
	}

	pub fn get_sample_rate(&self) -> i32 { self.fSampleRate as i32}
	
	pub fn class_init(sample_rate: i32) {
		// Obtaining locks on 0 static var(s)
	}
	pub fn instance_reset_params(&mut self) {
	}
	pub fn instance_clear(&mut self) {
		for l0 in 0..2 {
			self.fVec0[l0 as usize] = 0.0;
		}
		for l1 in 0..2 {
			self.fRec1[l1 as usize] = 0.0;
		}
		for l2 in 0..3 {
			self.fRec0[l2 as usize] = 0.0;
		}
		for l3 in 0..3 {
			self.fRec4[l3 as usize] = 0.0;
		}
		for l4 in 0..3 {
			self.fRec3[l4 as usize] = 0.0;
		}
		for l5 in 0..3 {
			self.fRec2[l5 as usize] = 0.0;
		}
		for l6 in 0..3 {
			self.fRec7[l6 as usize] = 0.0;
		}
		for l7 in 0..3 {
			self.fRec6[l7 as usize] = 0.0;
		}
		for l8 in 0..3 {
			self.fRec5[l8 as usize] = 0.0;
		}
		for l9 in 0..3 {
			self.fRec10[l9 as usize] = 0.0;
		}
		for l10 in 0..3 {
			self.fRec9[l10 as usize] = 0.0;
		}
		for l11 in 0..3 {
			self.fRec8[l11 as usize] = 0.0;
		}
		for l12 in 0..3 {
			self.fRec13[l12 as usize] = 0.0;
		}
		for l13 in 0..3 {
			self.fRec12[l13 as usize] = 0.0;
		}
		for l14 in 0..3 {
			self.fRec11[l14 as usize] = 0.0;
		}
		for l15 in 0..3 {
			self.fRec16[l15 as usize] = 0.0;
		}
		for l16 in 0..3 {
			self.fRec15[l16 as usize] = 0.0;
		}
		for l17 in 0..3 {
			self.fRec14[l17 as usize] = 0.0;
		}
		for l18 in 0..3 {
			self.fRec19[l18 as usize] = 0.0;
		}
		for l19 in 0..3 {
			self.fRec18[l19 as usize] = 0.0;
		}
		for l20 in 0..3 {
			self.fRec17[l20 as usize] = 0.0;
		}
		for l21 in 0..3 {
			self.fRec22[l21 as usize] = 0.0;
		}
		for l22 in 0..3 {
			self.fRec21[l22 as usize] = 0.0;
		}
		for l23 in 0..3 {
			self.fRec20[l23 as usize] = 0.0;
		}
		for l24 in 0..3 {
			self.fRec25[l24 as usize] = 0.0;
		}
		for l25 in 0..3 {
			self.fRec24[l25 as usize] = 0.0;
		}
		for l26 in 0..3 {
			self.fRec23[l26 as usize] = 0.0;
		}
		for l27 in 0..3 {
			self.fRec28[l27 as usize] = 0.0;
		}
		for l28 in 0..3 {
			self.fRec27[l28 as usize] = 0.0;
		}
		for l29 in 0..3 {
			self.fRec26[l29 as usize] = 0.0;
		}
		for l30 in 0..3 {
			self.fRec31[l30 as usize] = 0.0;
		}
		for l31 in 0..3 {
			self.fRec30[l31 as usize] = 0.0;
		}
		for l32 in 0..3 {
			self.fRec29[l32 as usize] = 0.0;
		}
		for l33 in 0..3 {
			self.fRec34[l33 as usize] = 0.0;
		}
		for l34 in 0..3 {
			self.fRec33[l34 as usize] = 0.0;
		}
		for l35 in 0..3 {
			self.fRec32[l35 as usize] = 0.0;
		}
		for l36 in 0..3 {
			self.fRec37[l36 as usize] = 0.0;
		}
		for l37 in 0..3 {
			self.fRec36[l37 as usize] = 0.0;
		}
		for l38 in 0..3 {
			self.fRec35[l38 as usize] = 0.0;
		}
		for l39 in 0..3 {
			self.fRec40[l39 as usize] = 0.0;
		}
		for l40 in 0..3 {
			self.fRec39[l40 as usize] = 0.0;
		}
		for l41 in 0..3 {
			self.fRec38[l41 as usize] = 0.0;
		}
		for l42 in 0..3 {
			self.fRec43[l42 as usize] = 0.0;
		}
		for l43 in 0..3 {
			self.fRec42[l43 as usize] = 0.0;
		}
		for l44 in 0..3 {
			self.fRec41[l44 as usize] = 0.0;
		}
		for l45 in 0..2 {
			self.fRec45[l45 as usize] = 0.0;
		}
		for l46 in 0..3 {
			self.fRec44[l46 as usize] = 0.0;
		}
	}
	pub fn instance_constants(&mut self, sample_rate: i32) {
		// Obtaining locks on 0 static var(s)
		self.fSampleRate = sample_rate;
		self.fConst0 = F32::min(1.92e+05, F32::max(1.0, (self.fSampleRate) as F32));
		self.fConst1 = F32::tan(314.15927 / self.fConst0);
		self.fConst2 = 1.0 / self.fConst1;
		self.fConst3 = 1.0 / ((self.fConst2 + 1.0) / self.fConst1 + 1.0);
		self.fConst4 = 1.0 / (self.fConst2 + 1.0);
		self.fConst5 = 1.0 - self.fConst2;
		self.fConst6 = (self.fConst2 + -1.0) / self.fConst1 + 1.0;
		self.fConst7 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst1));
		self.fConst8 = F32::tan(499.51324 / self.fConst0);
		self.fConst9 = 1.0 / self.fConst8;
		self.fConst10 = (self.fConst9 + 0.16666667) / self.fConst8 + 1.0;
		self.fConst11 = 0.19 / (self.fConst8 * self.fConst10);
		self.fConst12 = F32::tan(471.2389 / self.fConst0);
		self.fConst13 = 1.0 / self.fConst12;
		self.fConst14 = (self.fConst13 + 0.16666667) / self.fConst12 + 1.0;
		self.fConst15 = 0.19 / (self.fConst12 * self.fConst14);
		self.fConst16 = F32::tan(442.96457 / self.fConst0);
		self.fConst17 = 1.0 / self.fConst16;
		self.fConst18 = (self.fConst17 + 0.16666667) / self.fConst16 + 1.0;
		self.fConst19 = 0.19 / (self.fConst16 * self.fConst18);
		self.fConst20 = 1.0 / self.fConst18;
		self.fConst21 = (self.fConst17 + -0.16666667) / self.fConst16 + 1.0;
		self.fConst22 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst16));
		self.fConst23 = 1.0 / self.fConst14;
		self.fConst24 = (self.fConst13 + -0.16666667) / self.fConst12 + 1.0;
		self.fConst25 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst12));
		self.fConst26 = 1.0 / self.fConst10;
		self.fConst27 = (self.fConst9 + -0.16666667) / self.fConst8 + 1.0;
		self.fConst28 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst8));
		self.fConst29 = F32::tan(832.52203 / self.fConst0);
		self.fConst30 = 1.0 / self.fConst29;
		self.fConst31 = (self.fConst30 + 0.16666667) / self.fConst29 + 1.0;
		self.fConst32 = 0.19 / (self.fConst29 * self.fConst31);
		self.fConst33 = F32::tan(785.3982 / self.fConst0);
		self.fConst34 = 1.0 / self.fConst33;
		self.fConst35 = (self.fConst34 + 0.16666667) / self.fConst33 + 1.0;
		self.fConst36 = 0.19 / (self.fConst33 * self.fConst35);
		self.fConst37 = F32::tan(738.2743 / self.fConst0);
		self.fConst38 = 1.0 / self.fConst37;
		self.fConst39 = (self.fConst38 + 0.16666667) / self.fConst37 + 1.0;
		self.fConst40 = 0.19 / (self.fConst37 * self.fConst39);
		self.fConst41 = 1.0 / self.fConst39;
		self.fConst42 = (self.fConst38 + -0.16666667) / self.fConst37 + 1.0;
		self.fConst43 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst37));
		self.fConst44 = 1.0 / self.fConst35;
		self.fConst45 = (self.fConst34 + -0.16666667) / self.fConst33 + 1.0;
		self.fConst46 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst33));
		self.fConst47 = 1.0 / self.fConst31;
		self.fConst48 = (self.fConst30 + -0.16666667) / self.fConst29 + 1.0;
		self.fConst49 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst29));
		self.fConst50 = F32::tan(1165.5309 / self.fConst0);
		self.fConst51 = 1.0 / self.fConst50;
		self.fConst52 = (self.fConst51 + 0.16666667) / self.fConst50 + 1.0;
		self.fConst53 = 0.19 / (self.fConst50 * self.fConst52);
		self.fConst54 = F32::tan(1099.5574 / self.fConst0);
		self.fConst55 = 1.0 / self.fConst54;
		self.fConst56 = (self.fConst55 + 0.16666667) / self.fConst54 + 1.0;
		self.fConst57 = 0.19 / (self.fConst54 * self.fConst56);
		self.fConst58 = F32::tan(1033.584 / self.fConst0);
		self.fConst59 = 1.0 / self.fConst58;
		self.fConst60 = (self.fConst59 + 0.16666667) / self.fConst58 + 1.0;
		self.fConst61 = 0.19 / (self.fConst58 * self.fConst60);
		self.fConst62 = 1.0 / self.fConst60;
		self.fConst63 = (self.fConst59 + -0.16666667) / self.fConst58 + 1.0;
		self.fConst64 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst58));
		self.fConst65 = 1.0 / self.fConst56;
		self.fConst66 = (self.fConst55 + -0.16666667) / self.fConst54 + 1.0;
		self.fConst67 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst54));
		self.fConst68 = 1.0 / self.fConst52;
		self.fConst69 = (self.fConst51 + -0.16666667) / self.fConst50 + 1.0;
		self.fConst70 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst50));
		self.fConst71 = F32::tan(1665.0441 / self.fConst0);
		self.fConst72 = 1.0 / self.fConst71;
		self.fConst73 = (self.fConst72 + 0.16666667) / self.fConst71 + 1.0;
		self.fConst74 = 0.19 / (self.fConst71 * self.fConst73);
		self.fConst75 = F32::tan(1570.7964 / self.fConst0);
		self.fConst76 = 1.0 / self.fConst75;
		self.fConst77 = (self.fConst76 + 0.16666667) / self.fConst75 + 1.0;
		self.fConst78 = 0.19 / (self.fConst75 * self.fConst77);
		self.fConst79 = F32::tan(1476.5486 / self.fConst0);
		self.fConst80 = 1.0 / self.fConst79;
		self.fConst81 = (self.fConst80 + 0.16666667) / self.fConst79 + 1.0;
		self.fConst82 = 0.19 / (self.fConst79 * self.fConst81);
		self.fConst83 = 1.0 / self.fConst81;
		self.fConst84 = (self.fConst80 + -0.16666667) / self.fConst79 + 1.0;
		self.fConst85 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst79));
		self.fConst86 = 1.0 / self.fConst77;
		self.fConst87 = (self.fConst76 + -0.16666667) / self.fConst75 + 1.0;
		self.fConst88 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst75));
		self.fConst89 = 1.0 / self.fConst73;
		self.fConst90 = (self.fConst72 + -0.16666667) / self.fConst71 + 1.0;
		self.fConst91 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst71));
		self.fConst92 = F32::tan(2097.9556 / self.fConst0);
		self.fConst93 = 1.0 / self.fConst92;
		self.fConst94 = (self.fConst93 + 0.16666667) / self.fConst92 + 1.0;
		self.fConst95 = 0.19 / (self.fConst92 * self.fConst94);
		self.fConst96 = F32::tan(1979.2034 / self.fConst0);
		self.fConst97 = 1.0 / self.fConst96;
		self.fConst98 = (self.fConst97 + 0.16666667) / self.fConst96 + 1.0;
		self.fConst99 = 0.19 / (self.fConst96 * self.fConst98);
		self.fConst100 = F32::tan(1860.4512 / self.fConst0);
		self.fConst101 = 1.0 / self.fConst100;
		self.fConst102 = (self.fConst101 + 0.16666667) / self.fConst100 + 1.0;
		self.fConst103 = 0.19 / (self.fConst100 * self.fConst102);
		self.fConst104 = 1.0 / self.fConst102;
		self.fConst105 = (self.fConst101 + -0.16666667) / self.fConst100 + 1.0;
		self.fConst106 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst100));
		self.fConst107 = 1.0 / self.fConst98;
		self.fConst108 = (self.fConst97 + -0.16666667) / self.fConst96 + 1.0;
		self.fConst109 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst96));
		self.fConst110 = 1.0 / self.fConst94;
		self.fConst111 = (self.fConst93 + -0.16666667) / self.fConst92 + 1.0;
		self.fConst112 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst92));
		self.fConst113 = F32::tan(2664.0706 / self.fConst0);
		self.fConst114 = 1.0 / self.fConst113;
		self.fConst115 = (self.fConst114 + 0.16666667) / self.fConst113 + 1.0;
		self.fConst116 = 0.19 / (self.fConst113 * self.fConst115);
		self.fConst117 = F32::tan(2513.2742 / self.fConst0);
		self.fConst118 = 1.0 / self.fConst117;
		self.fConst119 = (self.fConst118 + 0.16666667) / self.fConst117 + 1.0;
		self.fConst120 = 0.19 / (self.fConst117 * self.fConst119);
		self.fConst121 = F32::tan(2362.4778 / self.fConst0);
		self.fConst122 = 1.0 / self.fConst121;
		self.fConst123 = (self.fConst122 + 0.16666667) / self.fConst121 + 1.0;
		self.fConst124 = 0.19 / (self.fConst121 * self.fConst123);
		self.fConst125 = 1.0 / self.fConst123;
		self.fConst126 = (self.fConst122 + -0.16666667) / self.fConst121 + 1.0;
		self.fConst127 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst121));
		self.fConst128 = 1.0 / self.fConst119;
		self.fConst129 = (self.fConst118 + -0.16666667) / self.fConst117 + 1.0;
		self.fConst130 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst117));
		self.fConst131 = 1.0 / self.fConst115;
		self.fConst132 = (self.fConst114 + -0.16666667) / self.fConst113 + 1.0;
		self.fConst133 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst113));
		self.fConst134 = F32::tan(3330.0881 / self.fConst0);
		self.fConst135 = 1.0 / self.fConst134;
		self.fConst136 = (self.fConst135 + 0.16666667) / self.fConst134 + 1.0;
		self.fConst137 = 0.19 / (self.fConst134 * self.fConst136);
		self.fConst138 = F32::tan(3141.5928 / self.fConst0);
		self.fConst139 = 1.0 / self.fConst138;
		self.fConst140 = (self.fConst139 + 0.16666667) / self.fConst138 + 1.0;
		self.fConst141 = 0.19 / (self.fConst138 * self.fConst140);
		self.fConst142 = F32::tan(2953.0972 / self.fConst0);
		self.fConst143 = 1.0 / self.fConst142;
		self.fConst144 = (self.fConst143 + 0.16666667) / self.fConst142 + 1.0;
		self.fConst145 = 0.19 / (self.fConst142 * self.fConst144);
		self.fConst146 = 1.0 / self.fConst144;
		self.fConst147 = (self.fConst143 + -0.16666667) / self.fConst142 + 1.0;
		self.fConst148 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst142));
		self.fConst149 = 1.0 / self.fConst140;
		self.fConst150 = (self.fConst139 + -0.16666667) / self.fConst138 + 1.0;
		self.fConst151 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst138));
		self.fConst152 = 1.0 / self.fConst136;
		self.fConst153 = (self.fConst135 + -0.16666667) / self.fConst134 + 1.0;
		self.fConst154 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst134));
		self.fConst155 = F32::tan(4329.1147 / self.fConst0);
		self.fConst156 = 1.0 / self.fConst155;
		self.fConst157 = (self.fConst156 + 0.16666667) / self.fConst155 + 1.0;
		self.fConst158 = 0.19 / (self.fConst155 * self.fConst157);
		self.fConst159 = F32::tan(4084.0706 / self.fConst0);
		self.fConst160 = 1.0 / self.fConst159;
		self.fConst161 = (self.fConst160 + 0.16666667) / self.fConst159 + 1.0;
		self.fConst162 = 0.19 / (self.fConst159 * self.fConst161);
		self.fConst163 = F32::tan(3839.0261 / self.fConst0);
		self.fConst164 = 1.0 / self.fConst163;
		self.fConst165 = (self.fConst164 + 0.16666667) / self.fConst163 + 1.0;
		self.fConst166 = 0.19 / (self.fConst163 * self.fConst165);
		self.fConst167 = 1.0 / self.fConst165;
		self.fConst168 = (self.fConst164 + -0.16666667) / self.fConst163 + 1.0;
		self.fConst169 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst163));
		self.fConst170 = 1.0 / self.fConst161;
		self.fConst171 = (self.fConst160 + -0.16666667) / self.fConst159 + 1.0;
		self.fConst172 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst159));
		self.fConst173 = 1.0 / self.fConst157;
		self.fConst174 = (self.fConst156 + -0.16666667) / self.fConst155 + 1.0;
		self.fConst175 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst155));
		self.fConst176 = F32::tan(5328.141 / self.fConst0);
		self.fConst177 = 1.0 / self.fConst176;
		self.fConst178 = (self.fConst177 + 0.16666667) / self.fConst176 + 1.0;
		self.fConst179 = 0.19 / (self.fConst176 * self.fConst178);
		self.fConst180 = F32::tan(5026.5483 / self.fConst0);
		self.fConst181 = 1.0 / self.fConst180;
		self.fConst182 = (self.fConst181 + 0.16666667) / self.fConst180 + 1.0;
		self.fConst183 = 0.19 / (self.fConst180 * self.fConst182);
		self.fConst184 = F32::tan(4724.9556 / self.fConst0);
		self.fConst185 = 1.0 / self.fConst184;
		self.fConst186 = (self.fConst185 + 0.16666667) / self.fConst184 + 1.0;
		self.fConst187 = 0.19 / (self.fConst184 * self.fConst186);
		self.fConst188 = 1.0 / self.fConst186;
		self.fConst189 = (self.fConst185 + -0.16666667) / self.fConst184 + 1.0;
		self.fConst190 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst184));
		self.fConst191 = 1.0 / self.fConst182;
		self.fConst192 = (self.fConst181 + -0.16666667) / self.fConst180 + 1.0;
		self.fConst193 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst180));
		self.fConst194 = 1.0 / self.fConst178;
		self.fConst195 = (self.fConst177 + -0.16666667) / self.fConst176 + 1.0;
		self.fConst196 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst176));
		self.fConst197 = F32::tan(6660.1763 / self.fConst0);
		self.fConst198 = 1.0 / self.fConst197;
		self.fConst199 = (self.fConst198 + 0.16666667) / self.fConst197 + 1.0;
		self.fConst200 = 0.19 / (self.fConst197 * self.fConst199);
		self.fConst201 = F32::tan(6283.1855 / self.fConst0);
		self.fConst202 = 1.0 / self.fConst201;
		self.fConst203 = (self.fConst202 + 0.16666667) / self.fConst201 + 1.0;
		self.fConst204 = 0.19 / (self.fConst201 * self.fConst203);
		self.fConst205 = F32::tan(5906.1943 / self.fConst0);
		self.fConst206 = 1.0 / self.fConst205;
		self.fConst207 = (self.fConst206 + 0.16666667) / self.fConst205 + 1.0;
		self.fConst208 = 0.19 / (self.fConst205 * self.fConst207);
		self.fConst209 = 1.0 / self.fConst207;
		self.fConst210 = (self.fConst206 + -0.16666667) / self.fConst205 + 1.0;
		self.fConst211 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst205));
		self.fConst212 = 1.0 / self.fConst203;
		self.fConst213 = (self.fConst202 + -0.16666667) / self.fConst201 + 1.0;
		self.fConst214 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst201));
		self.fConst215 = 1.0 / self.fConst199;
		self.fConst216 = (self.fConst198 + -0.16666667) / self.fConst197 + 1.0;
		self.fConst217 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst197));
		self.fConst218 = F32::tan(8658.2295 / self.fConst0);
		self.fConst219 = 1.0 / self.fConst218;
		self.fConst220 = (self.fConst219 + 0.16666667) / self.fConst218 + 1.0;
		self.fConst221 = 0.19 / (self.fConst218 * self.fConst220);
		self.fConst222 = F32::tan(8168.141 / self.fConst0);
		self.fConst223 = 1.0 / self.fConst222;
		self.fConst224 = (self.fConst223 + 0.16666667) / self.fConst222 + 1.0;
		self.fConst225 = 0.19 / (self.fConst222 * self.fConst224);
		self.fConst226 = F32::tan(7678.0522 / self.fConst0);
		self.fConst227 = 1.0 / self.fConst226;
		self.fConst228 = (self.fConst227 + 0.16666667) / self.fConst226 + 1.0;
		self.fConst229 = 0.19 / (self.fConst226 * self.fConst228);
		self.fConst230 = 1.0 / self.fConst228;
		self.fConst231 = (self.fConst227 + -0.16666667) / self.fConst226 + 1.0;
		self.fConst232 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst226));
		self.fConst233 = 1.0 / self.fConst224;
		self.fConst234 = (self.fConst223 + -0.16666667) / self.fConst222 + 1.0;
		self.fConst235 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst222));
		self.fConst236 = 1.0 / self.fConst220;
		self.fConst237 = (self.fConst219 + -0.16666667) / self.fConst218 + 1.0;
		self.fConst238 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst218));
		self.fConst239 = F32::tan(11655.309 / self.fConst0);
		self.fConst240 = 1.0 / self.fConst239;
		self.fConst241 = (self.fConst240 + 0.16666667) / self.fConst239 + 1.0;
		self.fConst242 = 0.19 / (self.fConst239 * self.fConst241);
		self.fConst243 = F32::tan(10995.574 / self.fConst0);
		self.fConst244 = 1.0 / self.fConst243;
		self.fConst245 = (self.fConst244 + 0.16666667) / self.fConst243 + 1.0;
		self.fConst246 = 0.19 / (self.fConst243 * self.fConst245);
		self.fConst247 = F32::tan(10335.84 / self.fConst0);
		self.fConst248 = 1.0 / self.fConst247;
		self.fConst249 = (self.fConst248 + 0.16666667) / self.fConst247 + 1.0;
		self.fConst250 = 0.19 / (self.fConst247 * self.fConst249);
		self.fConst251 = 1.0 / self.fConst249;
		self.fConst252 = (self.fConst248 + -0.16666667) / self.fConst247 + 1.0;
		self.fConst253 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst247));
		self.fConst254 = 1.0 / self.fConst245;
		self.fConst255 = (self.fConst244 + -0.16666667) / self.fConst243 + 1.0;
		self.fConst256 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst243));
		self.fConst257 = 1.0 / self.fConst241;
		self.fConst258 = (self.fConst240 + -0.16666667) / self.fConst239 + 1.0;
		self.fConst259 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst239));
		self.fConst260 = F32::tan(16650.441 / self.fConst0);
		self.fConst261 = 1.0 / self.fConst260;
		self.fConst262 = (self.fConst261 + 0.16666667) / self.fConst260 + 1.0;
		self.fConst263 = 0.19 / (self.fConst260 * self.fConst262);
		self.fConst264 = F32::tan(15707.963 / self.fConst0);
		self.fConst265 = 1.0 / self.fConst264;
		self.fConst266 = (self.fConst265 + 0.16666667) / self.fConst264 + 1.0;
		self.fConst267 = 0.19 / (self.fConst264 * self.fConst266);
		self.fConst268 = F32::tan(14765.485 / self.fConst0);
		self.fConst269 = 1.0 / self.fConst268;
		self.fConst270 = (self.fConst269 + 0.16666667) / self.fConst268 + 1.0;
		self.fConst271 = 0.19 / (self.fConst268 * self.fConst270);
		self.fConst272 = 1.0 / self.fConst270;
		self.fConst273 = (self.fConst269 + -0.16666667) / self.fConst268 + 1.0;
		self.fConst274 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst268));
		self.fConst275 = 1.0 / self.fConst266;
		self.fConst276 = (self.fConst265 + -0.16666667) / self.fConst264 + 1.0;
		self.fConst277 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst264));
		self.fConst278 = 1.0 / self.fConst262;
		self.fConst279 = (self.fConst261 + -0.16666667) / self.fConst260 + 1.0;
		self.fConst280 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst260));
		self.fConst281 = F32::tan(26640.705 / self.fConst0);
		self.fConst282 = 1.0 / self.fConst281;
		self.fConst283 = (self.fConst282 + 0.16666667) / self.fConst281 + 1.0;
		self.fConst284 = 0.19 / (self.fConst281 * self.fConst283);
		self.fConst285 = F32::tan(25132.742 / self.fConst0);
		self.fConst286 = 1.0 / self.fConst285;
		self.fConst287 = (self.fConst286 + 0.16666667) / self.fConst285 + 1.0;
		self.fConst288 = 0.19 / (self.fConst285 * self.fConst287);
		self.fConst289 = F32::tan(23624.777 / self.fConst0);
		self.fConst290 = 1.0 / self.fConst289;
		self.fConst291 = (self.fConst290 + 0.16666667) / self.fConst289 + 1.0;
		self.fConst292 = 0.19 / (self.fConst289 * self.fConst291);
		self.fConst293 = 1.0 / self.fConst291;
		self.fConst294 = (self.fConst290 + -0.16666667) / self.fConst289 + 1.0;
		self.fConst295 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst289));
		self.fConst296 = 1.0 / self.fConst287;
		self.fConst297 = (self.fConst286 + -0.16666667) / self.fConst285 + 1.0;
		self.fConst298 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst285));
		self.fConst299 = 1.0 / self.fConst283;
		self.fConst300 = (self.fConst282 + -0.16666667) / self.fConst281 + 1.0;
		self.fConst301 = 2.0 * (1.0 - 1.0 / Bank16_faustpower2_f(self.fConst281));
		self.fConst302 = F32::tan(31415.926 / self.fConst0);
		self.fConst303 = Bank16_faustpower2_f(self.fConst302);
		self.fConst304 = 1.0 / self.fConst302;
		self.fConst305 = (self.fConst304 + 1.0) / self.fConst302 + 1.0;
		self.fConst306 = 1.0 / (self.fConst303 * self.fConst305);
		self.fConst307 = 1.0 / (self.fConst304 + 1.0);
		self.fConst308 = 1.0 - self.fConst304;
		self.fConst309 = 1.0 / self.fConst305;
		self.fConst310 = (self.fConst304 + -1.0) / self.fConst302 + 1.0;
		self.fConst311 = 2.0 * (1.0 - 1.0 / self.fConst303);
	}
	pub fn instance_init(&mut self, sample_rate: i32) {
		self.instance_constants(sample_rate);
		self.instance_reset_params();
		self.instance_clear();
	}
	pub fn init(&mut self, sample_rate: i32) {
		Bank16::class_init(sample_rate);
		self.instance_init(sample_rate);
	}
	
	pub fn build_user_interface(&self, ui_interface: &mut dyn UI<FaustFloat>) {
		Self::build_user_interface_static(ui_interface);
	}
	
	pub fn build_user_interface_static(ui_interface: &mut dyn UI<FaustFloat>) {
		ui_interface.open_vertical_box("bank16");
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
		let [outputs0, outputs1, outputs2, outputs3, outputs4, outputs5, outputs6, outputs7, outputs8, outputs9, outputs10, outputs11, outputs12, outputs13, outputs14, outputs15, .. ] = outputs.as_mut() else { panic!("wrong number of output buffers"); };
		let outputs0 = outputs0.as_mut()[..count].iter_mut();
		let outputs1 = outputs1.as_mut()[..count].iter_mut();
		let outputs2 = outputs2.as_mut()[..count].iter_mut();
		let outputs3 = outputs3.as_mut()[..count].iter_mut();
		let outputs4 = outputs4.as_mut()[..count].iter_mut();
		let outputs5 = outputs5.as_mut()[..count].iter_mut();
		let outputs6 = outputs6.as_mut()[..count].iter_mut();
		let outputs7 = outputs7.as_mut()[..count].iter_mut();
		let outputs8 = outputs8.as_mut()[..count].iter_mut();
		let outputs9 = outputs9.as_mut()[..count].iter_mut();
		let outputs10 = outputs10.as_mut()[..count].iter_mut();
		let outputs11 = outputs11.as_mut()[..count].iter_mut();
		let outputs12 = outputs12.as_mut()[..count].iter_mut();
		let outputs13 = outputs13.as_mut()[..count].iter_mut();
		let outputs14 = outputs14.as_mut()[..count].iter_mut();
		let outputs15 = outputs15.as_mut()[..count].iter_mut();
		let zipped_iterators = inputs0.zip(outputs0).zip(outputs1).zip(outputs2).zip(outputs3).zip(outputs4).zip(outputs5).zip(outputs6).zip(outputs7).zip(outputs8).zip(outputs9).zip(outputs10).zip(outputs11).zip(outputs12).zip(outputs13).zip(outputs14).zip(outputs15);
		for ((((((((((((((((input0, output0), output1), output2), output3), output4), output5), output6), output7), output8), output9), output10), output11), output12), output13), output14), output15) in zipped_iterators {
			let mut fTemp0: F32 = (*input0) as F32;
			self.fVec0[0] = fTemp0;
			self.fRec1[0] = -(self.fConst4 * (self.fConst5 * self.fRec1[1] - (fTemp0 + self.fVec0[1])));
			self.fRec0[0] = self.fRec1[0] - self.fConst3 * (self.fConst6 * self.fRec0[2] + self.fConst7 * self.fRec0[1]);
			*output0 = (self.fConst3 * (self.fRec0[2] + self.fRec0[0] + 2.0 * self.fRec0[1])) as FaustFloat;
			self.fRec4[0] = fTemp0 - self.fConst20 * (self.fConst21 * self.fRec4[2] + self.fConst22 * self.fRec4[1]);
			self.fRec3[0] = self.fConst19 * (self.fRec4[0] - self.fRec4[2]) - self.fConst23 * (self.fConst24 * self.fRec3[2] + self.fConst25 * self.fRec3[1]);
			self.fRec2[0] = self.fConst15 * (self.fRec3[0] - self.fRec3[2]) - self.fConst26 * (self.fConst27 * self.fRec2[2] + self.fConst28 * self.fRec2[1]);
			*output1 = (self.fConst11 * (self.fRec2[0] - self.fRec2[2])) as FaustFloat;
			self.fRec7[0] = fTemp0 - self.fConst41 * (self.fConst42 * self.fRec7[2] + self.fConst43 * self.fRec7[1]);
			self.fRec6[0] = self.fConst40 * (self.fRec7[0] - self.fRec7[2]) - self.fConst44 * (self.fConst45 * self.fRec6[2] + self.fConst46 * self.fRec6[1]);
			self.fRec5[0] = self.fConst36 * (self.fRec6[0] - self.fRec6[2]) - self.fConst47 * (self.fConst48 * self.fRec5[2] + self.fConst49 * self.fRec5[1]);
			*output2 = (self.fConst32 * (self.fRec5[0] - self.fRec5[2])) as FaustFloat;
			self.fRec10[0] = fTemp0 - self.fConst62 * (self.fConst63 * self.fRec10[2] + self.fConst64 * self.fRec10[1]);
			self.fRec9[0] = self.fConst61 * (self.fRec10[0] - self.fRec10[2]) - self.fConst65 * (self.fConst66 * self.fRec9[2] + self.fConst67 * self.fRec9[1]);
			self.fRec8[0] = self.fConst57 * (self.fRec9[0] - self.fRec9[2]) - self.fConst68 * (self.fConst69 * self.fRec8[2] + self.fConst70 * self.fRec8[1]);
			*output3 = (self.fConst53 * (self.fRec8[0] - self.fRec8[2])) as FaustFloat;
			self.fRec13[0] = fTemp0 - self.fConst83 * (self.fConst84 * self.fRec13[2] + self.fConst85 * self.fRec13[1]);
			self.fRec12[0] = self.fConst82 * (self.fRec13[0] - self.fRec13[2]) - self.fConst86 * (self.fConst87 * self.fRec12[2] + self.fConst88 * self.fRec12[1]);
			self.fRec11[0] = self.fConst78 * (self.fRec12[0] - self.fRec12[2]) - self.fConst89 * (self.fConst90 * self.fRec11[2] + self.fConst91 * self.fRec11[1]);
			*output4 = (self.fConst74 * (self.fRec11[0] - self.fRec11[2])) as FaustFloat;
			self.fRec16[0] = fTemp0 - self.fConst104 * (self.fConst105 * self.fRec16[2] + self.fConst106 * self.fRec16[1]);
			self.fRec15[0] = self.fConst103 * (self.fRec16[0] - self.fRec16[2]) - self.fConst107 * (self.fConst108 * self.fRec15[2] + self.fConst109 * self.fRec15[1]);
			self.fRec14[0] = self.fConst99 * (self.fRec15[0] - self.fRec15[2]) - self.fConst110 * (self.fConst111 * self.fRec14[2] + self.fConst112 * self.fRec14[1]);
			*output5 = (self.fConst95 * (self.fRec14[0] - self.fRec14[2])) as FaustFloat;
			self.fRec19[0] = fTemp0 - self.fConst125 * (self.fConst126 * self.fRec19[2] + self.fConst127 * self.fRec19[1]);
			self.fRec18[0] = self.fConst124 * (self.fRec19[0] - self.fRec19[2]) - self.fConst128 * (self.fConst129 * self.fRec18[2] + self.fConst130 * self.fRec18[1]);
			self.fRec17[0] = self.fConst120 * (self.fRec18[0] - self.fRec18[2]) - self.fConst131 * (self.fConst132 * self.fRec17[2] + self.fConst133 * self.fRec17[1]);
			*output6 = (self.fConst116 * (self.fRec17[0] - self.fRec17[2])) as FaustFloat;
			self.fRec22[0] = fTemp0 - self.fConst146 * (self.fConst147 * self.fRec22[2] + self.fConst148 * self.fRec22[1]);
			self.fRec21[0] = self.fConst145 * (self.fRec22[0] - self.fRec22[2]) - self.fConst149 * (self.fConst150 * self.fRec21[2] + self.fConst151 * self.fRec21[1]);
			self.fRec20[0] = self.fConst141 * (self.fRec21[0] - self.fRec21[2]) - self.fConst152 * (self.fConst153 * self.fRec20[2] + self.fConst154 * self.fRec20[1]);
			*output7 = (self.fConst137 * (self.fRec20[0] - self.fRec20[2])) as FaustFloat;
			self.fRec25[0] = fTemp0 - self.fConst167 * (self.fConst168 * self.fRec25[2] + self.fConst169 * self.fRec25[1]);
			self.fRec24[0] = self.fConst166 * (self.fRec25[0] - self.fRec25[2]) - self.fConst170 * (self.fConst171 * self.fRec24[2] + self.fConst172 * self.fRec24[1]);
			self.fRec23[0] = self.fConst162 * (self.fRec24[0] - self.fRec24[2]) - self.fConst173 * (self.fConst174 * self.fRec23[2] + self.fConst175 * self.fRec23[1]);
			*output8 = (self.fConst158 * (self.fRec23[0] - self.fRec23[2])) as FaustFloat;
			self.fRec28[0] = fTemp0 - self.fConst188 * (self.fConst189 * self.fRec28[2] + self.fConst190 * self.fRec28[1]);
			self.fRec27[0] = self.fConst187 * (self.fRec28[0] - self.fRec28[2]) - self.fConst191 * (self.fConst192 * self.fRec27[2] + self.fConst193 * self.fRec27[1]);
			self.fRec26[0] = self.fConst183 * (self.fRec27[0] - self.fRec27[2]) - self.fConst194 * (self.fConst195 * self.fRec26[2] + self.fConst196 * self.fRec26[1]);
			*output9 = (self.fConst179 * (self.fRec26[0] - self.fRec26[2])) as FaustFloat;
			self.fRec31[0] = fTemp0 - self.fConst209 * (self.fConst210 * self.fRec31[2] + self.fConst211 * self.fRec31[1]);
			self.fRec30[0] = self.fConst208 * (self.fRec31[0] - self.fRec31[2]) - self.fConst212 * (self.fConst213 * self.fRec30[2] + self.fConst214 * self.fRec30[1]);
			self.fRec29[0] = self.fConst204 * (self.fRec30[0] - self.fRec30[2]) - self.fConst215 * (self.fConst216 * self.fRec29[2] + self.fConst217 * self.fRec29[1]);
			*output10 = (self.fConst200 * (self.fRec29[0] - self.fRec29[2])) as FaustFloat;
			self.fRec34[0] = fTemp0 - self.fConst230 * (self.fConst231 * self.fRec34[2] + self.fConst232 * self.fRec34[1]);
			self.fRec33[0] = self.fConst229 * (self.fRec34[0] - self.fRec34[2]) - self.fConst233 * (self.fConst234 * self.fRec33[2] + self.fConst235 * self.fRec33[1]);
			self.fRec32[0] = self.fConst225 * (self.fRec33[0] - self.fRec33[2]) - self.fConst236 * (self.fConst237 * self.fRec32[2] + self.fConst238 * self.fRec32[1]);
			*output11 = (self.fConst221 * (self.fRec32[0] - self.fRec32[2])) as FaustFloat;
			self.fRec37[0] = fTemp0 - self.fConst251 * (self.fConst252 * self.fRec37[2] + self.fConst253 * self.fRec37[1]);
			self.fRec36[0] = self.fConst250 * (self.fRec37[0] - self.fRec37[2]) - self.fConst254 * (self.fConst255 * self.fRec36[2] + self.fConst256 * self.fRec36[1]);
			self.fRec35[0] = self.fConst246 * (self.fRec36[0] - self.fRec36[2]) - self.fConst257 * (self.fConst258 * self.fRec35[2] + self.fConst259 * self.fRec35[1]);
			*output12 = (self.fConst242 * (self.fRec35[0] - self.fRec35[2])) as FaustFloat;
			self.fRec40[0] = fTemp0 - self.fConst272 * (self.fConst273 * self.fRec40[2] + self.fConst274 * self.fRec40[1]);
			self.fRec39[0] = self.fConst271 * (self.fRec40[0] - self.fRec40[2]) - self.fConst275 * (self.fConst276 * self.fRec39[2] + self.fConst277 * self.fRec39[1]);
			self.fRec38[0] = self.fConst267 * (self.fRec39[0] - self.fRec39[2]) - self.fConst278 * (self.fConst279 * self.fRec38[2] + self.fConst280 * self.fRec38[1]);
			*output13 = (self.fConst263 * (self.fRec38[0] - self.fRec38[2])) as FaustFloat;
			self.fRec43[0] = fTemp0 - self.fConst293 * (self.fConst294 * self.fRec43[2] + self.fConst295 * self.fRec43[1]);
			self.fRec42[0] = self.fConst292 * (self.fRec43[0] - self.fRec43[2]) - self.fConst296 * (self.fConst297 * self.fRec42[2] + self.fConst298 * self.fRec42[1]);
			self.fRec41[0] = self.fConst288 * (self.fRec42[0] - self.fRec42[2]) - self.fConst299 * (self.fConst300 * self.fRec41[2] + self.fConst301 * self.fRec41[1]);
			*output14 = (self.fConst284 * (self.fRec41[0] - self.fRec41[2])) as FaustFloat;
			self.fRec45[0] = -(self.fConst307 * (self.fConst308 * self.fRec45[1] - self.fConst304 * (fTemp0 - self.fVec0[1])));
			self.fRec44[0] = self.fRec45[0] - self.fConst309 * (self.fConst310 * self.fRec44[2] + self.fConst311 * self.fRec44[1]);
			*output15 = (self.fConst306 * (self.fRec44[2] + (self.fRec44[0] - 2.0 * self.fRec44[1]))) as FaustFloat;
			self.fVec0[1] = self.fVec0[0];
			self.fRec1[1] = self.fRec1[0];
			self.fRec0[2] = self.fRec0[1];
			self.fRec0[1] = self.fRec0[0];
			self.fRec4[2] = self.fRec4[1];
			self.fRec4[1] = self.fRec4[0];
			self.fRec3[2] = self.fRec3[1];
			self.fRec3[1] = self.fRec3[0];
			self.fRec2[2] = self.fRec2[1];
			self.fRec2[1] = self.fRec2[0];
			self.fRec7[2] = self.fRec7[1];
			self.fRec7[1] = self.fRec7[0];
			self.fRec6[2] = self.fRec6[1];
			self.fRec6[1] = self.fRec6[0];
			self.fRec5[2] = self.fRec5[1];
			self.fRec5[1] = self.fRec5[0];
			self.fRec10[2] = self.fRec10[1];
			self.fRec10[1] = self.fRec10[0];
			self.fRec9[2] = self.fRec9[1];
			self.fRec9[1] = self.fRec9[0];
			self.fRec8[2] = self.fRec8[1];
			self.fRec8[1] = self.fRec8[0];
			self.fRec13[2] = self.fRec13[1];
			self.fRec13[1] = self.fRec13[0];
			self.fRec12[2] = self.fRec12[1];
			self.fRec12[1] = self.fRec12[0];
			self.fRec11[2] = self.fRec11[1];
			self.fRec11[1] = self.fRec11[0];
			self.fRec16[2] = self.fRec16[1];
			self.fRec16[1] = self.fRec16[0];
			self.fRec15[2] = self.fRec15[1];
			self.fRec15[1] = self.fRec15[0];
			self.fRec14[2] = self.fRec14[1];
			self.fRec14[1] = self.fRec14[0];
			self.fRec19[2] = self.fRec19[1];
			self.fRec19[1] = self.fRec19[0];
			self.fRec18[2] = self.fRec18[1];
			self.fRec18[1] = self.fRec18[0];
			self.fRec17[2] = self.fRec17[1];
			self.fRec17[1] = self.fRec17[0];
			self.fRec22[2] = self.fRec22[1];
			self.fRec22[1] = self.fRec22[0];
			self.fRec21[2] = self.fRec21[1];
			self.fRec21[1] = self.fRec21[0];
			self.fRec20[2] = self.fRec20[1];
			self.fRec20[1] = self.fRec20[0];
			self.fRec25[2] = self.fRec25[1];
			self.fRec25[1] = self.fRec25[0];
			self.fRec24[2] = self.fRec24[1];
			self.fRec24[1] = self.fRec24[0];
			self.fRec23[2] = self.fRec23[1];
			self.fRec23[1] = self.fRec23[0];
			self.fRec28[2] = self.fRec28[1];
			self.fRec28[1] = self.fRec28[0];
			self.fRec27[2] = self.fRec27[1];
			self.fRec27[1] = self.fRec27[0];
			self.fRec26[2] = self.fRec26[1];
			self.fRec26[1] = self.fRec26[0];
			self.fRec31[2] = self.fRec31[1];
			self.fRec31[1] = self.fRec31[0];
			self.fRec30[2] = self.fRec30[1];
			self.fRec30[1] = self.fRec30[0];
			self.fRec29[2] = self.fRec29[1];
			self.fRec29[1] = self.fRec29[0];
			self.fRec34[2] = self.fRec34[1];
			self.fRec34[1] = self.fRec34[0];
			self.fRec33[2] = self.fRec33[1];
			self.fRec33[1] = self.fRec33[0];
			self.fRec32[2] = self.fRec32[1];
			self.fRec32[1] = self.fRec32[0];
			self.fRec37[2] = self.fRec37[1];
			self.fRec37[1] = self.fRec37[0];
			self.fRec36[2] = self.fRec36[1];
			self.fRec36[1] = self.fRec36[0];
			self.fRec35[2] = self.fRec35[1];
			self.fRec35[1] = self.fRec35[0];
			self.fRec40[2] = self.fRec40[1];
			self.fRec40[1] = self.fRec40[0];
			self.fRec39[2] = self.fRec39[1];
			self.fRec39[1] = self.fRec39[0];
			self.fRec38[2] = self.fRec38[1];
			self.fRec38[1] = self.fRec38[0];
			self.fRec43[2] = self.fRec43[1];
			self.fRec43[1] = self.fRec43[0];
			self.fRec42[2] = self.fRec42[1];
			self.fRec42[1] = self.fRec42[0];
			self.fRec41[2] = self.fRec41[1];
			self.fRec41[1] = self.fRec41[0];
			self.fRec45[1] = self.fRec45[0];
			self.fRec44[2] = self.fRec44[1];
			self.fRec44[1] = self.fRec44[0];
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

impl FaustDsp for Bank16 {
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
