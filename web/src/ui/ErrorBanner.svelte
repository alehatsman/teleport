<script lang="ts">
  // The .banner--error block (app.css) with the role="alert" that every
  // call site used to have to remember. Placement (margins) is the
  // caller's: wrap it if the default .banner margin is wrong there.
  let { message }: { message: string } = $props()
</script>

<div class="banner banner--error" role="alert">{message}</div>
