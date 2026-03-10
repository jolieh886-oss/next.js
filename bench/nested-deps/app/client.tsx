'use client'

import { foo } from './actions'

export default function Client({ inline }) {
  return (
    <div>
      <button onClick={foo}>imported</button>
      <button onClick={inline}>inline</button>
    </div>
  )
}
