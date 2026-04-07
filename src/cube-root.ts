import { Experimental, Field, Gadgets } from 'o1js';

const { ZkFunction } = Experimental;

export function cubeRoot64Main(x: Field, y: Field) {
  Gadgets.rangeCheck64(y);
  y.square().mul(y).assertEquals(x);
}

export const cubeRoot64 = ZkFunction({
  name: 'cube-root-64',
  publicInputType: Field,
  privateInputTypes: [Field],
  main: cubeRoot64Main,
});
