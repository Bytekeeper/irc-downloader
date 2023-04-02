<script>
  let downloads = [];
  function updateDownloads() {
    fetch("/downloads")
      .then((response) => response.json())
      .then((json) => {
        json.forEach(item => {
          let download = downloads.find(dl => dl.id === item.id)
          if (download !== undefined && download.status.Progress?.transferred !== undefined) {
            item.bps = item.status.Progress.transferred - download.status.Progress.transferred;
          }
        });
        downloads = json;
      });
  }
  setInterval(updateDownloads, 1000);

  let searchQuery = '';
  let searchResults = [];
  function perform_search() {
    fetch("/search?query=" + encodeURIComponent(searchQuery))
      .then((response) => response.json())
      .then((results) => searchResults = results);
    searchQuery = "";
  }

  let messages = [];
  let evtSource = new EventSource("/events");
  evtSource.addEventListener("irc-message", (event) => {
    messages = messages.slice(messages.length - 99);
    messages.push(JSON.parse(event.data));
  });

  function download(nick, command, fileName, server) {
    fetch("/download", {
      method: "POST", 
      body: JSON.stringify(
        { nick: nick, command: command, fileName: fileName, server: server }
      ),
      headers: {
        "Content-Type": "application/json"
      }
    }).then(() => updateDownloads());
  }

  function abortDownload(id) {
    fetch("/download/" + encodeURIComponent(id), {
      method: "DELETE", 
      body: JSON.stringify(
        { id: id }
      ),
      headers: {
        "Content-Type": "application/json"
      }
    }).then(() => updateDownloads());
  }
</script>

<main class="max-w-7xl mx-auto px-4 text-slate-300">
  <div>
    <h1 class="text-3xl font-extrabold border-b-4 border-indigo-500">Downloads</h1>
    <ul>
    {#each downloads as download}
      <li>{download.fileName} from <span class="nickname">{download.nick}</span> 
        {#if download.status.Progress}
          <progress value={download.status.Progress.transferred}
                    max={download.status.Progress.file_size}>
                    {download.status.Progress.transferred} / {download.status.Progress.file_size}
          </progress>{new Intl.NumberFormat(undefined, {maximumFractionDigits: 2}).format(download.bps / 1024)} KBps
        {:else if download.status == "Requested"}
          <span class="py-1 px-1 rounded-lg bg-green-700">Requested</span>
        {:else if download.status == "Delayed"}
          <span class="py-1 px-1 rounded-lg bg-neutral-700">Delayed</span>
        {:else if download.status == "SenderAbsent"}
          <span class="py-1 px-1 rounded-lg bg-red-700">Unavailable</span>
        {:else if download.status.Failed}
          <span class="py-1 px-1 rounded-lg bg-red-600">Failed: {download.status.Failed}</span>
        {/if}
        <button class="btn-danger" on:click={() => abortDownload(download.id)}>Abort</button>
    {/each}
    </ul>
  </div>
  <div>
    <h1 class="flex gap-4 flex-row text-3xl font-extrabold border-b-4 border-indigo-500">
      <div class="flex-none">Search</div>
      <form class="grow" on:submit|preventDefault="{perform_search}">
        <input class="rounded-lg py-1 bg-slate-700 w-full my-1" bind:value={searchQuery} />
      </form>
    </h1>
  </div>
  <div>
    <ul class="space-y-1">
    {#each searchResults as result}
      <li><button class="btn-primary" on:click={() => download(result.nick, result.command, result.fileName, result.server)}>Download</button><span class="server-name">{result.server}</span><span class="nickname">{result.nick}</span> {result.fileName}
    {/each}
    </ul>
  </div>
  <div>
    <details>
      <summary class="text-3xl font-extrabold border-b-4 border-indigo-500">Log</summary>
      <ul>
      {#each messages as message}
        <li>{message.prefix} : {message.message}
      {/each}
      </ul>
    </details>
  </div>
</main>

<style lang="postcss">
  .logo {
    height: 6em;
    padding: 1.5em;
    will-change: filter;
    transition: filter 300ms;
  }
  .logo:hover {
    filter: drop-shadow(0 0 2em #646cffaa);
  }
  .logo.svelte:hover {
    filter: drop-shadow(0 0 2em #ff3e00aa);
  }
  .read-the-docs {
    color: #888;
  }
</style>
