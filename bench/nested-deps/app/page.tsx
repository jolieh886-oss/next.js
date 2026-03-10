import Client from './client'

export default function Page() {
  return (
    <Client
      inline={async () => {
        'use server'
        console.log('inline')
      }}
    />
  )
}
